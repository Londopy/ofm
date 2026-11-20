mod audio;
mod calibrate;
mod config;
mod host;
mod kissport;
mod ptt;
mod sim;
mod station;
mod stop;
mod updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, ProfileName, PttMethod};
use ofm_core::chansim::{Channel, ChannelConfig};
use ofm_core::modem::Profile;
use std::io::Write;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    version,
    about = "Open FM sound card modem with a VARA FM compatible host interface"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the modem (the default).
    Run(RunArgs),
    /// List audio devices.
    Devices,
    /// Send a level-setting tone and show the receive level.
    Calibrate(RunArgs),
    /// Run two modems joined by a simulated radio link, to try a client without radios.
    Sim(SimArgs),
    /// Check GitHub Releases for a newer ofm and install it after asking.
    Update,
}

#[derive(clap::Args, Clone, Default)]
struct RunArgs {
    /// Command port; the data port is the next one up.
    #[arg(long)]
    port: Option<u16>,
    /// KISS port for connectionless frames.
    #[arg(long)]
    kiss_port: Option<u16>,
    #[arg(long, value_enum)]
    profile: Option<ProfileArg>,
    /// Input device, matched by part of its name.
    #[arg(long)]
    input: Option<String>,
    /// Output device, matched by part of its name.
    #[arg(long)]
    output: Option<String>,
}

#[derive(clap::Args, Clone)]
struct SimArgs {
    /// Command port of the first modem; its data port is one up and its KISS port ten up.
    /// The second modem's ports are 100 higher.
    #[arg(long, default_value_t = 8300, value_parser = clap::value_parser!(u16).range(1..=65400))]
    port: u16,
    #[arg(long, value_enum)]
    profile: Option<ProfileArg>,
    /// Channel SNR in dB, measured over the whole audio band, from -10 to 60.
    #[arg(long, default_value_t = 30.0, value_parser = snr_db)]
    snr: f32,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProfileArg {
    Narrow,
    Wide,
}

fn apply(cfg: &mut Config, args: &RunArgs) {
    if let Some(p) = args.port {
        cfg.host.port = p;
    }
    if let Some(p) = args.kiss_port {
        cfg.kiss.port = p;
    }
    if let Some(p) = args.profile {
        cfg.radio.profile = match p {
            ProfileArg::Narrow => ProfileName::Narrow,
            ProfileArg::Wide => ProfileName::Wide,
        };
    }
    if args.input.is_some() {
        cfg.audio.input.clone_from(&args.input);
    }
    if args.output.is_some() {
        cfg.audio.output.clone_from(&args.output);
    }
}

fn snr_db(text: &str) -> Result<f32, String> {
    match text.parse::<f32>() {
        Ok(db) if (-10.0..=60.0).contains(&db) => Ok(db),
        _ => Err(format!("{text:?} is not a number of dB from -10 to 60")),
    }
}

fn profile(cfg: &Config) -> Profile {
    match cfg.radio.profile {
        ProfileName::Narrow => Profile::Narrow,
        ProfileName::Wide => Profile::Wide,
    }
}

fn open_ptt(cfg: &Config) -> Result<Box<dyn ptt::Ptt>> {
    let p = &cfg.ptt;
    Ok(match p.method {
        PttMethod::Client | PttMethod::Vox => Box::new(ptt::External),
        PttMethod::Rts => Box::new(ptt::Serial::open(&p.serial_port, ptt::Line::Rts)?),
        PttMethod::Dtr => Box::new(ptt::Serial::open(&p.serial_port, ptt::Line::Dtr)?),
        PttMethod::Cm108 => Box::new(ptt::Cm108::open(p.cm108_path.as_deref(), p.cm108_gpio)?),
        PttMethod::Rigctld => Box::new(ptt::Rigctld::connect(&p.rigctld)?),
    })
}

struct Setup {
    audio: Box<dyn station::Audio>,
    audio_ok: bool,
    ptt: Box<dyn ptt::Ptt>,
    port: u16,
    kiss_port: u16,
    name: String,
    ptt_lines: bool,
}

fn start_station(cfg: &Config, setup: Setup) -> Result<station::Station> {
    let port = setup.port;
    let (host_tx, host_rx) = channel();
    let (cmd, data) = host::serve(port, host_tx)
        .with_context(|| format!("opening ports {} and {}", port, port + 1))?;
    let kiss = if cfg.kiss.enabled {
        let (tx, rx) = channel();
        let clients = kissport::serve(setup.kiss_port, tx)
            .with_context(|| format!("opening KISS port {}", setup.kiss_port))?;
        Some(station::Kiss {
            inbox: rx,
            out: Box::new(clients),
        })
    } else {
        None
    };
    eprintln!(
        "{}ofm {} {} profile, command port {}, data port {}{}",
        setup.name,
        env!("CARGO_PKG_VERSION"),
        profile(cfg).name(),
        port,
        port + 1,
        if cfg.kiss.enabled {
            format!(", KISS port {}", setup.kiss_port)
        } else {
            String::new()
        }
    );
    let station_cfg = station::StationConfig {
        profile: profile(cfg),
        txdelay_ms: cfg.radio.txdelay_ms,
        papr_db: cfg.radio.papr_db,
        tx_gain: cfg.audio.tx_gain,
        cwid: cfg.radio.cwid,
        max_retries: cfg.host.max_retries,
        kiss_level: cfg.kiss.level,
        name: setup.name,
        ptt_lines: setup.ptt_lines,
    };
    let io = station::StationIo {
        audio: setup.audio,
        audio_ok: setup.audio_ok,
        ptt: setup.ptt,
        host: host_rx,
        cmd,
        data,
        kiss,
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64
        ^ u64::from(port);
    Ok(station::Station::new(station_cfg, io, seed))
}

// steps every station until ctrl-c; dropping them on the way out releases PTT
fn drive(mut stations: Vec<station::Station>) {
    let start = Instant::now();
    while !stop::requested() {
        let now = start.elapsed().as_millis() as u64;
        for st in &mut stations {
            st.step(now);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("stopping");
}

fn check_for_update(cfg: &Config) {
    if cfg.updater.check_on_start {
        std::thread::spawn(|| {
            if let Ok(Some(u)) = updater::check() {
                eprintln!(
                    "ofm {} is available, run `ofm update` to install it",
                    u.version
                );
            }
        });
    }
}

fn run(cfg: Config) -> Result<()> {
    let (audio, audio_ok): (Box<dyn station::Audio>, bool) =
        match audio::SoundCard::open(cfg.audio.input.as_deref(), cfg.audio.output.as_deref()) {
            Ok(card) => (Box::new(card), true),
            Err(e) => {
                eprintln!("no sound card: {e:#}");
                (Box::new(audio::NoAudio), false)
            }
        };
    let st = start_station(
        &cfg,
        Setup {
            audio,
            audio_ok,
            ptt: open_ptt(&cfg)?,
            port: cfg.host.port,
            kiss_port: cfg.kiss.port,
            name: String::new(),
            ptt_lines: true,
        },
    )?;
    check_for_update(&cfg);
    drive(vec![st]);
    Ok(())
}

fn sim(cfg: Config, args: &SimArgs) -> Result<()> {
    let mut chan = match profile(&cfg) {
        Profile::Narrow => ChannelConfig::narrow(),
        Profile::Wide => ChannelConfig::wide(),
    };
    chan.snr_db = Some(args.snr);
    // the two sound cards drift apart a little, and each radio loses the start of its keyup
    chan.clock_ppm = 40.0;
    chan.keyup_ms = 30.0;
    let back = ChannelConfig {
        clock_ppm: -40.0,
        seed: 2,
        ..chan.clone()
    };
    let (a, b) = sim::pair(Channel::new(chan), Channel::new(back));
    let mut stations = Vec::new();
    for (audio, offset) in [(a, 0), (b, 100)] {
        let port = args.port + offset;
        stations.push(start_station(
            &cfg,
            Setup {
                audio: Box::new(audio),
                audio_ok: true,
                ptt: Box::new(ptt::External),
                port,
                kiss_port: port + 10,
                name: format!("[{port}] "),
                ptt_lines: false,
            },
        )?);
    }
    eprintln!("simulated radio link at {} dB SNR; ctrl-c stops", args.snr);
    drive(stations);
    Ok(())
}

fn update() -> Result<()> {
    let Some(u) = updater::check()? else {
        println!("ofm {} is the latest release", env!("CARGO_PKG_VERSION"));
        return Ok(());
    };
    println!(
        "ofm {} is available ({}). install it? [y/N] ",
        u.version, u.page
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        updater::install(&u)?;
        println!("updated to {}; restart ofm to use it", u.version);
    }
    Ok(())
}

// runs a command that can key the radio; ctrl-c or closing the console stops it, and its
// stations are dropped (releasing PTT) before windows is allowed to end the process
fn keying(command: impl FnOnce() -> Result<()>) -> Result<()> {
    stop::install()?;
    let result = command();
    stop::finished();
    result
}

fn load(args: &RunArgs) -> Result<Config> {
    let mut cfg = Config::load()?;
    apply(&mut cfg, args);
    Ok(cfg)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Devices) => audio::list(),
        Some(Cmd::Update) => update(),
        Some(Cmd::Calibrate(args)) => keying(|| {
            let mut cfg = load(&args)?;
            let ptt = open_ptt(&cfg)?;
            let p = profile(&cfg);
            calibrate::run(&mut cfg, p, ptt)
        }),
        Some(Cmd::Sim(args)) => keying(|| {
            let profile_only = RunArgs {
                profile: args.profile,
                ..RunArgs::default()
            };
            sim(load(&profile_only)?, &args)
        }),
        Some(Cmd::Run(args)) => keying(|| run(load(&args)?)),
        None => keying(|| run(load(&cli.run)?)),
    }
}
