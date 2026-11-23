// the modem's main loop: audio in, receiver, link, audio out, and the host ports

use crate::host::{Command, HostInput, Listening, Port};
use crate::ptt::Ptt;
use ofm_core::arq::{HostEvent, Link, LinkConfig, Out, Sense};
use ofm_core::kiss;
use ofm_core::modem::{build_frame, lead_in, Chunk, Header, Kind, Profile, Receiver, RxEvent, FS};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver as Inbox, Sender};

const ALIVE_MS: u64 = 60_000;
const CW_WPM: f32 = 20.0;
const CW_TONE: f32 = 800.0;
// past the end of the audio, PTT is dropped even if the output never says it finished
const TX_SLACK_MS: u64 = 2000;
const UNKEY_RETRY_MS: u64 = 1000;
const INPUT_LOST_MS: u64 = 3000;
// a CW ID still waiting for a quiet channel after this long is dropped
const CW_EXPIRE_MS: u64 = 30_000;
const LEVEL_EVERY_MS: u64 = 100;

/// Sound card side as the station sees it, 48 kHz mono.
pub trait Audio {
    /// Audio captured since the last call.
    fn capture(&mut self) -> Vec<f32>;
    /// Queues audio for playback and returns at once.
    fn play(&mut self, samples: Vec<f32>);
    /// True until everything queued has been played.
    fn playing(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct StationConfig {
    pub profile: Profile,
    pub txdelay_ms: u32,
    pub papr_db: f32,
    pub tx_gain: f32,
    pub cwid: bool,
    pub max_retries: u32,
    /// Level used for connectionless frames from the KISS port.
    pub kiss_level: u8,
    /// Put before each line this station logs; empty when there is only one.
    pub name: String,
    /// Tell the host PTT ON and PTT OFF. Off in `ofm sim`, where a client set up to key a
    /// real radio on those lines would transmit every simulated frame.
    pub ptt_lines: bool,
}

/// What a station tells a window that watches it.
#[derive(Clone, Debug, PartialEq)]
pub enum Monitor {
    /// A line for the host, as Pat sees it. PTT lines come here even when the host is not
    /// sent them.
    Line(String),
    /// A line the station logged.
    Log(String),
    /// RMS of the audio just captured, 0 to 1.
    Level(f32),
    /// The host's command connection closed.
    ClientLeft,
}

/// Write side of the KISS clients.
pub trait KissOut {
    fn deliver(&self, frame: &[u8]);
}

pub struct Station {
    cfg: StationConfig,
    link: Link,
    rx: Receiver,
    audio: Box<dyn Audio>,
    audio_ok: bool,
    ptt: Box<dyn Ptt>,
    host: Inbox<HostInput>,
    cmd: Port,
    data: Port,
    kiss: Option<Kiss>,
    kiss_queue: VecDeque<Vec<u8>>,
    tx_queue: VecDeque<(Vec<f32>, bool)>,
    // CW IDs and the earliest time each may go
    cw_queue: VecDeque<(Vec<f32>, u64)>,
    transmitting: Option<Keyed>,
    // a PTT release failed; try again at this time
    unkey_at: Option<u64>,
    last_logged: String,
    // last time the input device gave us any audio, and whether we said it went quiet
    last_input: u64,
    input_lost: bool,
    mycalls: Vec<String>,
    busy: bool,
    last_alive: u64,
    warned: bool,
    monitor: Option<Sender<Monitor>>,
    last_level: u64,
    // dropped last, after Drop has released PTT
    _listening: Vec<Listening>,
}

struct Keyed {
    from_link: bool,
    until: u64,
}

/// The KISS port: frames from clients, and where heard frames go.
pub struct Kiss {
    pub inbox: Inbox<Vec<u8>>,
    pub out: Box<dyn KissOut>,
}

/// Everything the station talks to besides its config.
pub struct StationIo {
    pub audio: Box<dyn Audio>,
    /// False when no sound card opened; the host is told MISSING SOUNDCARD.
    pub audio_ok: bool,
    pub ptt: Box<dyn Ptt>,
    pub host: Inbox<HostInput>,
    pub cmd: Port,
    pub data: Port,
    pub kiss: Option<Kiss>,
    /// The servers behind `cmd`, `data` and the KISS port; they close with the station.
    pub listening: Vec<Listening>,
    pub monitor: Option<Sender<Monitor>>,
}

impl Station {
    pub fn new(cfg: StationConfig, io: StationIo, seed: u64) -> Station {
        let link = Link::new(
            LinkConfig {
                profile: cfg.profile,
                txdelay_ms: cfg.txdelay_ms,
                max_retries: cfg.max_retries,
            },
            seed,
        );

        Station {
            rx: Receiver::new(cfg.profile),
            cfg,
            link,
            audio: io.audio,
            audio_ok: io.audio_ok,
            ptt: io.ptt,
            host: io.host,
            cmd: io.cmd,
            data: io.data,
            kiss: io.kiss,
            kiss_queue: VecDeque::new(),
            tx_queue: VecDeque::new(),
            cw_queue: VecDeque::new(),
            transmitting: None,
            unkey_at: None,
            last_logged: String::new(),
            last_input: 0,
            input_lost: false,
            mycalls: Vec::new(),
            busy: false,
            last_alive: 0,
            warned: false,
            monitor: io.monitor,
            last_level: 0,
            _listening: io.listening,
        }
    }

    /// One pass of the loop. Call it every few milliseconds with a monotonic clock.
    pub fn step(&mut self, now: u64) {
        while let Ok(input) = self.host.try_recv() {
            self.on_host(now, input);
        }
        if let Some(k) = &self.kiss {
            while let Ok(frame) = k.inbox.try_recv() {
                self.kiss_queue.push_back(frame);
            }
        }
        if !self.audio_ok && !self.warned && self.cmd.is_attached() {
            self.tell("MISSING SOUNDCARD");
            self.warned = true;
        }

        if self.unkey_at.is_some_and(|t| now >= t) {
            self.unkey(now);
        }
        let heard = self.audio.capture();
        self.watch_input(now, heard.is_empty());
        // while keyed the radio's receive audio is muted or is our own signal
        let keyed = self.transmitting.is_some();
        if !keyed && !heard.is_empty() && now >= self.last_level + LEVEL_EVERY_MS {
            self.last_level = now;
            let rms = (heard.iter().map(|x| x * x).sum::<f32>() / heard.len() as f32).sqrt();
            self.watched(Monitor::Level(rms));
        }
        match &self.transmitting {
            Some(k) if !self.audio.playing() || now >= k.until => {
                if self.audio.playing() {
                    self.log("audio output stopped mid transmission; dropping PTT");
                }
                let from_link = k.from_link;
                self.unkey(now);
                self.ptt_line(false);
                self.transmitting = None;
                // whatever we captured while keyed was our own signal
                self.rx.clear();
                if from_link {
                    self.link.tx_done(now);
                }
            }
            Some(_) => {}
            None => {
                for ev in self.rx.push(&heard) {
                    if let RxEvent::Frame(f) = ev {
                        if f.header.kind == Kind::Ui {
                            if let (Some(k), Some(p)) = (&self.kiss, kiss::ui_payload(&f)) {
                                k.out.deliver(&p);
                            }
                        } else {
                            self.link.on_frame(now, &f);
                        }
                    }
                }
            }
        }

        let busy = self.rx.busy();
        if busy != self.busy {
            self.busy = busy;
            self.tell(if busy { "BUSY ON" } else { "BUSY OFF" });
        }
        let keyed = self.transmitting.is_some();
        let sense = Sense {
            receiving: self.rx.receiving() || keyed,
            energy: self.rx.energy() || keyed,
            busy: busy || keyed,
        };
        self.link.tick(now, sense);
        while let Some(out) = self.link.poll() {
            self.on_link(now, out);
        }
        self.send_kiss();
        self.start_tx(now);

        if now >= self.last_alive + ALIVE_MS {
            self.last_alive = now;
            self.tell("IAMALIVE");
        }
    }

    fn on_host(&mut self, now: u64, input: HostInput) {
        match input {
            HostInput::Data(bytes) => self.link.send(now, &bytes),
            HostInput::Closed => {
                self.link.abort();
                self.link.set_listen(false);
                self.warned = false;
                self.watched(Monitor::ClientLeft);
            }
            HostInput::Command(c) => self.on_command(now, c),
        }
    }

    fn on_command(&mut self, now: u64, c: Command) {
        match c {
            Command::MyCall(calls) => {
                self.tell("OK");
                self.tell(&format!("REGISTERED {}", calls[0]));
                self.mycalls = calls.clone();
                self.link.set_mycalls(calls);
            }
            Command::Listen(on) => {
                self.tell("OK");
                self.link.set_listen(on);
            }
            Command::Connect { src, dst } => {
                if self.mycalls.iter().any(|c| c.eq_ignore_ascii_case(&src)) {
                    self.tell("OK");
                    self.link.connect(now, &src, &dst);
                } else {
                    self.tell("WRONG");
                }
            }
            Command::Disconnect => {
                self.tell("OK");
                self.link.disconnect(now);
            }
            Command::Abort => {
                self.tell("OK");
                self.link.abort();
                self.tx_queue.clear();
            }
            Command::CwId(on) => {
                self.cfg.cwid = on;
                self.tell("OK");
            }
            Command::Version => {
                self.tell(&format!("VERSION ofm {}", env!("CARGO_PKG_VERSION")));
            }
            Command::Compression | Command::Public | Command::Ignored => self.tell("OK"),
            Command::Wrong => self.tell("WRONG"),
        }
    }

    // a sound card that stops delivering audio makes the modem deaf without any error
    fn watch_input(&mut self, now: u64, empty: bool) {
        if !self.audio_ok {
            return;
        }
        if !empty {
            if self.input_lost {
                self.log("audio from the input device is back");
                self.input_lost = false;
            }
            self.last_input = now;
        } else if !self.input_lost && now >= self.last_input + INPUT_LOST_MS {
            self.log(&format!(
                "no audio from the input device for {} s; check that it is still plugged in",
                INPUT_LOST_MS / 1000
            ));
            self.input_lost = true;
            // the meter would otherwise go on showing the last level it heard
            self.watched(Monitor::Level(0.0));
        }
    }

    fn unkey(&mut self, now: u64) {
        match self.ptt.set(false) {
            Ok(()) => self.unkey_at = None,
            Err(e) => {
                self.log(&format!("ptt off failed, trying again: {e}"));
                self.unkey_at = Some(now + UNKEY_RETRY_MS);
            }
        }
    }

    fn on_link(&mut self, now: u64, out: Out) {
        match out {
            Out::Transmit(frames) => {
                let audio = self.frames_audio(&frames);
                self.tx_queue.push_back((audio, true));
            }
            Out::CwId { call, wait_ms } => {
                if self.cfg.cwid {
                    // silence first so the first dit is not lost while the radio keys up
                    let mut audio = vec![0.0; FS * self.cfg.txdelay_ms as usize / 1000];
                    audio.extend(ofm_core::cw::cw(&call, CW_WPM, CW_TONE, 0.5));
                    self.cw_queue.push_back((audio, now + wait_ms));
                }
            }
            Out::Host(e) => self.on_event(e),
        }
    }

    fn on_event(&mut self, e: HostEvent) {
        let line = match e {
            HostEvent::Pending => "PENDING".to_string(),
            HostEvent::CancelPending => "CANCELPENDING".to_string(),
            HostEvent::Connected { src, dst, profile } => {
                format!("CONNECTED {src} {dst} {}", profile.name())
            }
            HostEvent::Disconnected => "DISCONNECTED".to_string(),
            HostEvent::Buffer(n) => format!("BUFFER {n}"),
            HostEvent::Bitrate { level, bps } => format!("BITRATE ({level}) {bps} BPS"),
            HostEvent::Snr(db) => format!("SN {db:.1}"),
            HostEvent::Data(bytes) => {
                self.data.write(&bytes);
                return;
            }
        };
        if self.worth_logging(&line) {
            self.log(&line);
        }
        self.tell(&line);
    }

    fn tell(&self, line: &str) {
        self.cmd.line(line);
        self.watched(Monitor::Line(line.to_string()));
    }

    fn ptt_line(&self, on: bool) {
        let line = if on { "PTT ON" } else { "PTT OFF" };
        if self.cfg.ptt_lines {
            self.cmd.line(line);
        }
        self.watched(Monitor::Line(line.to_string()));
    }

    fn log(&self, text: &str) {
        eprintln!("{}{text}", self.cfg.name);
        self.watched(Monitor::Log(text.to_string()));
    }

    fn watched(&self, m: Monitor) {
        if let Some(tx) = &self.monitor {
            // a window that has closed no longer needs to hear
            let _ = tx.send(m);
        }
    }

    // link changes, not the per-frame chatter, and nothing twice in a row (the speed is
    // reported after every frame, and a DISCONNECT after the link closed is answered again)
    fn worth_logging(&mut self, line: &str) -> bool {
        if line.starts_with("BUFFER") || line.starts_with("SN ") || self.last_logged == line {
            return false;
        }
        self.last_logged = line.to_string();
        true
    }

    fn frames_audio(&self, frames: &[(Header, Vec<Chunk>)]) -> Vec<f32> {
        let mut audio = lead_in(self.cfg.txdelay_ms);
        for (h, chunks) in frames {
            audio.extend(build_frame(h, chunks, self.cfg.papr_db));
        }
        // short tail so the radio's audio filters ring out before PTT drops
        audio.extend(std::iter::repeat_n(0.0, 960));
        audio
    }

    // connectionless frames wait for a clear channel and for no link to be up
    fn send_kiss(&mut self) {
        if self.link.is_connected() || self.busy || self.transmitting.is_some() {
            return;
        }
        let Some(src) = self.mycalls.first().cloned() else {
            return;
        };
        while let Some(payload) = self.kiss_queue.pop_front() {
            match kiss::ui_frame(&src, &payload, self.cfg.profile, self.cfg.kiss_level) {
                Some(frame) => {
                    let audio = self.frames_audio(&[frame]);
                    self.tx_queue.push_back((audio, false));
                }
                None => self.log(&format!(
                    "kiss frame of {} bytes is too big for one frame",
                    payload.len()
                )),
            }
        }
    }

    // frames go first; a CW ID waits its turn and for a quiet channel
    fn start_tx(&mut self, now: u64) {
        if self.transmitting.is_some() {
            return;
        }
        while self
            .cw_queue
            .front()
            .is_some_and(|c| now > c.1 + CW_EXPIRE_MS)
        {
            self.cw_queue.pop_front();
        }
        let cw_ready = !self.busy && self.cw_queue.front().is_some_and(|c| now >= c.1);
        let next = match self.tx_queue.pop_front() {
            Some(t) => Some(t),
            None if cw_ready => self.cw_queue.pop_front().map(|(a, _)| (a, false)),
            None => None,
        };
        let Some((audio, from_link)) = next else {
            return;
        };
        if let Err(e) = self.ptt.set(true) {
            self.log(&format!("ptt on failed: {e}"));
        }
        self.unkey_at = None;
        self.ptt_line(true);
        let until = now + (audio.len() * 1000 / FS) as u64 + TX_SLACK_MS;
        let gain = self.cfg.tx_gain;
        self.audio.play(
            audio
                .into_iter()
                .map(|x| (x * gain).clamp(-1.0, 1.0))
                .collect(),
        );
        self.transmitting = Some(Keyed { from_link, until });
    }
}

// leaving the loop while keyed, on ctrl-c or a panic, must not leave the transmitter keyed
impl Drop for Station {
    fn drop(&mut self) {
        if self.transmitting.is_some() || self.unkey_at.is_some() {
            if let Err(e) = self.ptt.set(false) {
                self.log(&format!("ptt off failed on exit: {e}"));
            }
        }
    }
}

#[cfg(test)]
#[path = "station_tests.rs"]
mod tests;
