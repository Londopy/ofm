// building a station from the config: sound card, PTT, host and KISS ports, and the loop
// that drives it

use crate::audio::{NoAudio, SoundCard};
use crate::config::{Config, ProfileName, PttMethod};
use crate::station::{Audio, Kiss, Monitor, Station, StationConfig, StationIo};
use crate::{host, kissport, ptt};
use anyhow::{Context, Result};
use ofm_core::modem::Profile;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

pub fn profile(cfg: &Config) -> Profile {
    match cfg.radio.profile {
        ProfileName::Narrow => Profile::Narrow,
        ProfileName::Wide => Profile::Wide,
    }
}

pub fn open_ptt(cfg: &Config) -> Result<Box<dyn ptt::Ptt>> {
    let p = &cfg.ptt;
    Ok(match p.method {
        PttMethod::Client | PttMethod::Vox => Box::new(ptt::External),
        PttMethod::Rts => Box::new(ptt::Serial::open(&p.serial_port, ptt::Line::Rts)?),
        PttMethod::Dtr => Box::new(ptt::Serial::open(&p.serial_port, ptt::Line::Dtr)?),
        PttMethod::Cm108 => Box::new(ptt::Cm108::open(p.cm108_path.as_deref(), p.cm108_gpio)?),
        PttMethod::Rigctld => Box::new(ptt::Rigctld::connect(&p.rigctld)?),
    })
}

/// The configured sound card, or a silent stand-in and the reason when it can't be opened,
/// so the host still gets its ports and is told MISSING SOUNDCARD.
pub fn sound_card(cfg: &Config) -> (Box<dyn Audio>, Option<String>) {
    match SoundCard::open(cfg.audio.input.as_deref(), cfg.audio.output.as_deref()) {
        Ok(card) => (Box::new(card), None),
        Err(e) => {
            let problem = format!("{e:#}");
            eprintln!("no sound card: {problem}");
            (Box::new(NoAudio), Some(problem))
        }
    }
}

pub struct Setup {
    pub audio: Box<dyn Audio>,
    pub audio_ok: bool,
    pub ptt: Box<dyn ptt::Ptt>,
    pub port: u16,
    pub kiss_port: u16,
    pub name: String,
    pub ptt_lines: bool,
    pub monitor: Option<Sender<Monitor>>,
}

pub fn start_station(cfg: &Config, setup: Setup) -> Result<Station> {
    let port = setup.port;
    let (host_tx, host_rx) = channel();
    let (cmd, data, host_listening) = host::serve(port, host_tx).with_context(|| {
        format!(
            "opening ports {port} and {} (is another modem using them?)",
            port.saturating_add(1)
        )
    })?;
    let mut listening = vec![host_listening];
    let kiss = if cfg.kiss.enabled {
        let (tx, rx) = channel();
        let (clients, kiss_listening) = kissport::serve(setup.kiss_port, tx)
            .with_context(|| format!("opening KISS port {}", setup.kiss_port))?;
        listening.push(kiss_listening);
        Some(Kiss {
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
    let station_cfg = StationConfig {
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
    let io = StationIo {
        audio: setup.audio,
        audio_ok: setup.audio_ok,
        ptt: setup.ptt,
        host: host_rx,
        cmd,
        data,
        kiss,
        listening,
        monitor: setup.monitor,
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64
        ^ u64::from(port);
    Ok(Station::new(station_cfg, io, seed))
}

/// Steps every station until `stop` says so. Dropping them on the way out releases PTT and
/// closes their ports.
pub fn drive(mut stations: Vec<Station>, stop: impl Fn() -> bool) {
    let start = Instant::now();
    while !stop() {
        let now = start.elapsed().as_millis() as u64;
        for st in &mut stations {
            st.step(now);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
