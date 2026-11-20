use super::*;
use crate::host;
use crate::ptt::External;
use ofm_core::chansim::{Channel, ChannelConfig};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BLOCK: usize = 480;

// audio between two stations; each step moves 10 ms of virtual time
struct SimAudio {
    side: usize,
    air: Arc<Mutex<[VecDeque<f32>; 2]>>,
    channel: Channel,
    remaining: usize,
}

impl Audio for SimAudio {
    fn capture(&mut self) -> Vec<f32> {
        self.remaining = self.remaining.saturating_sub(BLOCK);
        let mut air = self.air.lock().unwrap();
        let q = &mut air[self.side];
        let mut out: Vec<f32> = q.drain(..q.len().min(BLOCK)).collect();
        out.resize(BLOCK, 0.0);
        out
    }

    fn play(&mut self, samples: Vec<f32>) {
        self.remaining = samples.len();
        let heard = self.channel.transmit(&samples);
        self.air.lock().unwrap()[1 - self.side].extend(heard);
    }

    fn playing(&self) -> bool {
        self.remaining > 0
    }
}

struct Client {
    cmd: TcpStream,
    data: TcpStream,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Client {
    fn connect(port: u16) -> Client {
        let cmd = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let data = TcpStream::connect(("127.0.0.1", port + 1)).unwrap();
        data.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (mut reader, store) = (cmd.try_clone().unwrap(), lines.clone());
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                // like Pat: split each read on CR, no carry-over, so a line split across
                // writes would show up here as two broken lines
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                assert!(text.ends_with('\r'), "partial line written: {text:?}");
                for l in text.split('\r').filter(|l| !l.is_empty()) {
                    store.lock().unwrap().push(l.to_string());
                }
            }
        });
        Client { cmd, data, lines }
    }

    fn send(&mut self, text: &str) {
        self.cmd.write_all(text.as_bytes()).unwrap();
    }

    fn wait_for(&self, what: impl Fn(&str) -> bool, secs: u64) -> String {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(l) = self.lines.lock().unwrap().iter().find(|l| what(l)) {
                return l.clone();
            }
            assert!(
                Instant::now() < end,
                "no matching line in {:?}",
                self.lines.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_data(&mut self, n: usize, secs: u64) -> Vec<u8> {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while got.len() < n {
            assert!(
                Instant::now() < end,
                "data port gave {} of {n} bytes",
                got.len()
            );
            if let Ok(k @ 1..) = self.data.read(&mut buf) {
                got.extend_from_slice(&buf[..k]);
            }
        }
        got
    }
}

fn station(
    port: u16,
    side: usize,
    air: Arc<Mutex<[VecDeque<f32>; 2]>>,
    chan: ChannelConfig,
) -> Station {
    let (tx, rx) = channel();
    let (cmd, data) = host::serve(port, tx).unwrap();
    let cfg = StationConfig {
        profile: Profile::Wide,
        txdelay_ms: 150,
        papr_db: 9.0,
        tx_gain: 1.0,
        cwid: true,
        max_retries: 10,
        kiss_level: 3,
        name: String::new(),
    };
    let audio = SimAudio {
        side,
        air,
        channel: Channel::new(chan),
        remaining: 0,
    };
    let io = StationIo {
        audio: Box::new(audio),
        audio_ok: true,
        ptt: Box::new(External),
        host: rx,
        cmd,
        data,
        kiss: None,
    };
    Station::new(cfg, io, side as u64 + 5)
}

fn pattern(n: usize, salt: u8) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 253) as u8 ^ salt).collect()
}

#[test]
fn pat_session_over_simulated_radio() {
    let base = 20_000 + (std::process::id() % 2000) as u16 * 4;
    let air = Arc::new(Mutex::new([VecDeque::new(), VecDeque::new()]));
    let mut chan = ChannelConfig::wide();
    chan.snr_db = Some(20.0);
    chan.clock_ppm = 80.0;
    chan.keyup_ms = 40.0;
    let mut a = station(base, 0, air.clone(), chan.clone());
    chan.clock_ppm = -80.0;
    chan.seed = 2;
    let mut b = station(base + 2, 1, air, chan);
    let pat = thread::spawn(move || {
        let mut pa = Client::connect(base);
        let mut pb = Client::connect(base + 2);
        // pat-vara's startup sequence for varafm, written without waiting for replies
        pa.send("PUBLIC ON\rCOMPRESSION TEXT\rMYCALL N0CALL\rLISTEN OFF\r");
        pb.send("PUBLIC ON\rCOMPRESSION TEXT\rMYCALL W1AW-10\rLISTEN OFF\rLISTEN ON\r");
        pa.send("VERSION\r");
        let v = pa.wait_for(|l| l.starts_with("VERSION"), 5);
        assert_eq!(v, format!("VERSION ofm {}", env!("CARGO_PKG_VERSION")));
        pa.wait_for(|l| l == "REGISTERED N0CALL", 5);
        pb.wait_for(|l| l == "REGISTERED W1AW-10", 5);

        pa.send("CONNECT N0CALL W1AW-10\r");
        for (p, me) in [(&pa, "N0CALL"), (&pb, "W1AW-10")] {
            let line = p.wait_for(|l| l.starts_with("CONNECTED "), 60);
            let f: Vec<&str> = line.split(' ').collect();
            assert!(f.len() >= 3, "{line}");
            assert!(f[1] == me || f[2] == me, "{line} does not name {me}");
            assert_eq!(&f[1..], ["N0CALL", "W1AW-10", "WIDE"]);
        }

        let (to_b, to_a) = (pattern(12_000, 1), pattern(4_000, 2));
        pa.data.write_all(&to_b).unwrap();
        pb.data.write_all(&to_a).unwrap();
        assert_eq!(pb.read_data(to_b.len(), 120), to_b);
        assert_eq!(pa.read_data(to_a.len(), 120), to_a);
        pa.wait_for(|l| l == "BUFFER 0", 60);

        pa.send("DISCONNECT\r");
        pa.wait_for(|l| l == "DISCONNECTED", 60);
        pb.wait_for(|l| l == "DISCONNECTED", 60);

        let allowed = [
            "OK",
            "WRONG",
            "PENDING",
            "CANCELPENDING",
            "CONNECTED",
            "DISCONNECTED",
            "BUFFER",
            "PTT",
            "BUSY",
            "REGISTERED",
            "IAMALIVE",
            "VERSION",
            "SN",
            "BITRATE",
        ];
        for p in [&pa, &pb] {
            for l in p.lines.lock().unwrap().iter() {
                let word = l.split(' ').next().unwrap();
                assert!(allowed.contains(&word), "unexpected line {l:?}");
            }
        }
        let bitrates = pa
            .lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("BITRATE"))
            .count();
        assert!(bitrates > 0);
    });
    let mut now = 0u64;
    while !pat.is_finished() {
        now += 10;
        a.step(now);
        b.step(now);
        thread::sleep(Duration::from_micros(200));
    }
    pat.join().unwrap();
}

#[test]
fn answers_every_command_pat_might_send() {
    let base = 24_000 + (std::process::id() % 2000) as u16 * 2;
    let air = Arc::new(Mutex::new([VecDeque::new(), VecDeque::new()]));
    let mut st = station(base, 0, air, ChannelConfig::wide());
    let mut p = Client::connect(base);
    p.send("MYCALL N0CALL N0CALL-1\rCWID OFF\rBW2300\rWINLINK SESSION\rCHAT ON\rFROB\rCONNECT K1ABC W1AW\r");
    let end = Instant::now() + Duration::from_secs(5);
    while p.lines.lock().unwrap().len() < 8 && Instant::now() < end {
        st.step(0);
        thread::sleep(Duration::from_millis(5));
    }
    let lines = p.lines.lock().unwrap().clone();
    assert_eq!(
        lines,
        [
            "OK",
            "REGISTERED N0CALL",
            "OK",
            "OK",
            "OK",
            "OK",
            "WRONG",
            "WRONG"
        ]
    );
}

// an output that never reports the end of its audio, as when a USB sound card is pulled
struct StuckAudio;

impl Audio for StuckAudio {
    fn capture(&mut self) -> Vec<f32> {
        vec![0.0; BLOCK]
    }

    fn play(&mut self, _samples: Vec<f32>) {}

    fn playing(&self) -> bool {
        true
    }
}

// records every PTT change; the first release fails
struct FlakyPtt {
    log: Arc<Mutex<Vec<(u64, bool)>>>,
    now: Arc<Mutex<u64>>,
    failed: bool,
}

impl Ptt for FlakyPtt {
    fn set(&mut self, on: bool) -> anyhow::Result<()> {
        let now = *self.now.lock().unwrap();
        if !on && !self.failed {
            self.failed = true;
            anyhow::bail!("rigctld timed out");
        }
        self.log.lock().unwrap().push((now, on));
        Ok(())
    }
}

#[test]
fn ptt_is_released_when_the_output_stalls_and_the_first_release_fails() {
    let base = 26_000 + (std::process::id() % 2000) as u16 * 2;
    let (tx, rx) = channel();
    let (cmd, data) = host::serve(base, tx).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let clock = Arc::new(Mutex::new(0u64));
    let ptt = FlakyPtt {
        log: log.clone(),
        now: clock.clone(),
        failed: false,
    };
    let cfg = StationConfig {
        profile: Profile::Wide,
        txdelay_ms: 150,
        papr_db: 9.0,
        tx_gain: 1.0,
        cwid: false,
        max_retries: 10,
        kiss_level: 3,
        name: String::new(),
    };
    let io = StationIo {
        audio: Box::new(StuckAudio),
        audio_ok: true,
        ptt: Box::new(ptt),
        host: rx,
        cmd,
        data,
        kiss: None,
    };
    let mut st = Station::new(cfg, io, 9);
    st.link.set_mycalls(vec!["N0CALL".into()]);
    st.link.connect(0, "N0CALL", "W1AW");
    for now in (10..4_000).step_by(10) {
        *clock.lock().unwrap() = now;
        st.step(now);
    }
    let log = log.lock().unwrap().clone();
    assert_eq!(log[0], (10, true), "{log:?}");
    let (off_at, on) = log[1];
    assert!(!on, "{log:?}");
    // a connect frame is well under a second, then 2 s of slack, then the 1 s retry
    assert!((3_000..3_600).contains(&off_at), "released at {off_at}");
    assert!(st.transmitting.is_none() && st.unkey_at.is_none());
}

// the spec asks for 6 kbps on Narrow on a cabled bench; this is the simulated version
#[test]
fn narrow_link_carries_about_6_kbps_one_way() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let base = 28_000 + (std::process::id() % 1000) as u16 * 4;
    let air = Arc::new(Mutex::new([VecDeque::new(), VecDeque::new()]));
    let mut chan = ChannelConfig::narrow();
    chan.snr_db = Some(30.0);
    chan.clock_ppm = 50.0;
    chan.keyup_ms = 30.0;
    let mk = |port, side, chan: ChannelConfig, air| {
        let mut st = station(port, side, air, chan);
        st.cfg.profile = Profile::Narrow;
        st.cfg.txdelay_ms = 200;
        st.rx = Receiver::new(Profile::Narrow);
        st.link = Link::new(
            LinkConfig {
                profile: Profile::Narrow,
                txdelay_ms: 200,
                max_retries: 10,
            },
            side as u64 + 5,
        );
        st
    };
    let mut a = mk(base, 0, chan.clone(), air.clone());
    chan.clock_ppm = -50.0;
    chan.seed = 2;
    let mut b = mk(base + 2, 1, chan, air);
    let clock = Arc::new(AtomicU64::new(0));
    let c2 = clock.clone();
    let pat = thread::spawn(move || {
        let mut pa = Client::connect(base);
        let mut pb = Client::connect(base + 2);
        pa.send("MYCALL N0CALL\r");
        pb.send("MYCALL W1AW-10\rLISTEN ON\r");
        pb.wait_for(|l| l == "REGISTERED W1AW-10", 5);
        pa.send("CONNECT N0CALL W1AW-10\r");
        pb.wait_for(|l| l.starts_with("CONNECTED "), 60);
        let data = pattern(30_000, 1);
        let t0 = c2.load(Ordering::SeqCst);
        pa.data.write_all(&data).unwrap();
        assert_eq!(pb.read_data(data.len(), 600), data);
        let bps = data.len() as u64 * 8000 / (c2.load(Ordering::SeqCst) - t0);
        assert!(bps >= 5_800, "{bps} bps");
    });
    let mut now = 0u64;
    while !pat.is_finished() {
        now += 10;
        clock.store(now, Ordering::SeqCst);
        a.step(now);
        b.step(now);
        thread::sleep(Duration::from_micros(300));
    }
    pat.join().unwrap();
}
