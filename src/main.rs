mod audio;
mod calibrate;
mod config;
mod host;
mod kissport;
mod launch;
mod ptt;
mod sim;
mod station;
mod stop;
mod updater;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, ProfileName};
use launch::{drive, open_ptt, profile, sound_card, start_station, Setup};
use ofm_core::chansim::{Channel, ChannelConfig};
use ofm_core::modem::Profile;
use std::io::Write;

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

#[derive(clap::Args, Clone, Default, PartialEq)]
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

#[derive(Clone, Copy, PartialEq, ValueEnum)]
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
    let (audio, problem) = sound_card(&cfg);
    let st = start_station(
        &cfg,
        Setup {
            audio,
            audio_ok: problem.is_none(),
            ptt: open_ptt(&cfg)?,
            port: cfg.host.port,
            kiss_port: cfg.kiss.port,
            name: String::new(),
            ptt_lines: true,
            monitor: None,
        },
    )?;
    check_for_update(&cfg);
    drive(vec![st], stop::requested);
    eprintln!("stopping");
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
                monitor: None,
            },
        )?);
    }
    eprintln!("simulated radio link at {} dB SNR; ctrl-c stops", args.snr);
    drive(stations, stop::requested);
    eprintln!("stopping");
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
