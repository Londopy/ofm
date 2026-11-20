mod audio;
mod calibrate;
mod config;
mod host;
mod kissport;
mod ptt;
mod station;
mod updater;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, ProfileName, PttMethod};
use ofm_core::modem::Profile;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
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

// set on ctrl-c (and SIGTERM or SIGHUP on Linux) so the loops can unkey and return
fn stop_flag() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || {
        // a second press means the loop is stuck; leave anyway
        if flag.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    })
    .context("installing the ctrl-c handler")?;
    Ok(stop)
}

fn run(cfg: Config) -> Result<()> {
    let stop = stop_flag()?;
    let (audio, audio_ok): (Box<dyn station::Audio>, bool) =
        match audio::SoundCard::open(cfg.audio.input.as_deref(), cfg.audio.output.as_deref()) {
            Ok(card) => (Box::new(card), true),
            Err(e) => {
                eprintln!("no sound card: {e:#}");
                (Box::new(audio::NoAudio), false)
            }
        };
    let ptt = open_ptt(&cfg)?;

    let (host_tx, host_rx) = channel();
    let (cmd, data) = host::serve(cfg.host.port, host_tx)
        .with_context(|| format!("opening ports {} and {}", cfg.host.port, cfg.host.port + 1))?;
    let kiss = if cfg.kiss.enabled {
        let (tx, rx) = channel();
        let clients = kissport::serve(cfg.kiss.port, tx)
            .with_context(|| format!("opening KISS port {}", cfg.kiss.port))?;
        Some(station::Kiss {
            inbox: rx,
            out: Box::new(clients),
        })
    } else {
        None
    };
    eprintln!(
        "ofm {} {} profile, command port {}, data port {}{}",
        env!("CARGO_PKG_VERSION"),
        profile(&cfg).name(),
        cfg.host.port,
        cfg.host.port + 1,
        if cfg.kiss.enabled {
            format!(", KISS port {}", cfg.kiss.port)
        } else {
            String::new()
        }
    );

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

    let station_cfg = station::StationConfig {
        profile: profile(&cfg),
        txdelay_ms: cfg.radio.txdelay_ms,
        papr_db: cfg.radio.papr_db,
        tx_gain: cfg.audio.tx_gain,
        cwid: cfg.radio.cwid,
        max_retries: cfg.host.max_retries,
        kiss_level: cfg.kiss.level,
    };
    let io = station::StationIo {
        audio,
        audio_ok,
        ptt,
        host: host_rx,
        cmd,
        data,
        kiss,
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let mut st = station::Station::new(station_cfg, io, seed);
    let start = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        st.step(start.elapsed().as_millis() as u64);
        std::thread::sleep(Duration::from_millis(5));
    }
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Devices) => audio::list(),
        Some(Cmd::Update) => update(),
        Some(Cmd::Calibrate(args)) => {
            let mut cfg = Config::load()?;
            apply(&mut cfg, &args);
            let ptt = open_ptt(&cfg)?;
            let p = profile(&cfg);
            let stop = stop_flag()?;
            calibrate::run(&mut cfg, p, ptt, &stop)
        }
        Some(Cmd::Run(args)) => {
            let mut cfg = Config::load()?;
            apply(&mut cfg, &args);
            run(cfg)
        }
        None => {
            let mut cfg = Config::load()?;
            apply(&mut cfg, &cli.run);
            run(cfg)
        }
    }
}
