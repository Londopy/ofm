// the modem the window runs: started on its own thread from the saved config, stopped
// (releasing PTT and closing its ports) before a restart or when the window closes

use crate::config::Config;
use crate::launch::{drive, open_ptt, sound_card, start_station, Setup};
use crate::station::Monitor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;

/// How starting the modem went.
pub enum Started {
    /// Running. Carries the sound card problem when it runs without one.
    Running(Option<String>),
    Failed(String),
}

pub struct Modem {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub started: Receiver<Started>,
    pub events: Receiver<Monitor>,
}

impl Modem {
    pub fn start(cfg: Config) -> Modem {
        let (started_tx, started) = channel();
        let (events_tx, events) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        let thread = std::thread::spawn(move || {
            let ptt = match open_ptt(&cfg) {
                Ok(p) => p,
                Err(e) => {
                    let _ = started_tx.send(Started::Failed(format!("PTT: {e:#}")));
                    return;
                }
            };
            let (audio, problem) = sound_card(&cfg);
            let station = start_station(
                &cfg,
                Setup {
                    audio,
                    audio_ok: problem.is_none(),
                    ptt,
                    port: cfg.host.port,
                    kiss_port: cfg.kiss.port,
                    name: String::new(),
                    ptt_lines: true,
                    monitor: Some(events_tx),
                },
            );
            match station {
                Ok(st) => {
                    let _ = started_tx.send(Started::Running(problem));
                    drive(vec![st], || {
                        quit.load(Ordering::SeqCst) || crate::stop::requested()
                    });
                }
                Err(e) => {
                    let _ = started_tx.send(Started::Failed(format!("{e:#}")));
                }
            }
        });
        Modem {
            stop,
            thread: Some(thread),
            started,
            events,
        }
    }
}

impl Drop for Modem {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    // a card that does not exist, so the test never opens this machine's sound card
    fn config(port: u16) -> Config {
        let mut cfg = Config::default();
        cfg.host.port = port;
        cfg.kiss.enabled = false;
        cfg.audio.input = Some("no such input device".into());
        cfg
    }

    #[test]
    fn a_restarted_modem_takes_the_same_ports_and_answers_the_client_again() {
        // a port the OS calls free can be taken by another test before the modem binds it,
        // so the first round tries until one sticks; later rounds must get the same one
        let mut port = 0;
        for round in 0..3 {
            let mut tries = 0;
            let (m, started) = loop {
                tries += 1;
                assert!(tries <= 20, "no free pair of ports in 20 tries");
                if round == 0 {
                    port = TcpListener::bind("127.0.0.1:0")
                        .and_then(|l| l.local_addr())
                        .unwrap()
                        .port();
                }
                let m = Modem::start(config(port));
                let started = m.started.recv_timeout(Duration::from_secs(10)).unwrap();
                if round > 0 || matches!(started, Started::Running(_)) {
                    break (m, started);
                }
            };
            let why = match &started {
                Started::Running(p) => format!("running, card problem {p:?}"),
                Started::Failed(e) => format!("failed: {e}"),
            };
            assert!(
                matches!(started, Started::Running(Some(_))),
                "round {round}: not running: {why}"
            );
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            c.write_all(b"VERSION\r").unwrap();
            let mut heard = String::new();
            let mut buf = [0u8; 256];
            while !heard.contains("VERSION ofm") {
                let n = c.read(&mut buf).unwrap();
                assert!(n > 0, "round {round}: closed before answering: {heard:?}");
                heard.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            // the window hears the same line, a moment after the client
            let mut seen: Vec<Monitor> = Vec::new();
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while !seen
                .iter()
                .any(|e| matches!(e, Monitor::Line(l) if l.starts_with("VERSION ofm")))
            {
                assert!(
                    std::time::Instant::now() < end,
                    "round {round}: window never heard it: {seen:?}"
                );
                seen.extend(m.events.recv_timeout(Duration::from_millis(50)));
            }
            drop(m);
            // whatever was still in flight, then the end of the connection
            loop {
                match c.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        break
                    }
                    other => panic!("round {round}: client still connected: {other:?}"),
                }
            }
        }
    }
}
