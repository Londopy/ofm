// what the window shows about the modem, folded from the same lines Pat is sent

use crate::calibrate::dbfs;
use crate::station::Monitor;
use std::collections::VecDeque;

const LOG_LINES: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Idle,
    /// A connect is under way, ours or another station's.
    Pending,
    /// `src` is the station that called.
    Connected {
        src: String,
        dst: String,
        profile: String,
    },
}

#[derive(Clone, Debug)]
pub struct Status {
    pub link: Link,
    /// The callsign the client registered with MYCALL.
    pub mycall: Option<String>,
    /// Speed level and its bit rate.
    pub level: Option<(u8, u32)>,
    pub snr: Option<f32>,
    /// Bytes the client has queued that the other station has not acknowledged yet.
    pub buffer: usize,
    pub busy: bool,
    pub ptt: bool,
    rx: f32,
    /// Seconds since the modem started, and the line.
    pub log: VecDeque<(u64, String)>,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            link: Link::Idle,
            mycall: None,
            level: None,
            snr: None,
            buffer: 0,
            busy: false,
            ptt: false,
            rx: 0.0,
            log: VecDeque::new(),
        }
    }
}

impl Status {
    pub fn apply(&mut self, at_secs: u64, m: Monitor) {
        match m {
            Monitor::Line(l) => self.line(&l),
            Monitor::Log(text) => self.note(at_secs, describe(&text)),
            Monitor::Level(rms) => self.rx = 0.6 * self.rx + 0.4 * rms,
            Monitor::ClientLeft => self.mycall = None,
        }
    }

    pub fn note(&mut self, at_secs: u64, text: String) {
        self.log.push_back((at_secs, text));
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    fn line(&mut self, l: &str) {
        let words: Vec<&str> = l.split_whitespace().collect();
        match words.as_slice() {
            ["REGISTERED", call] => self.mycall = Some(call.to_string()),
            ["PENDING"] => {
                if self.link == Link::Idle {
                    self.link = Link::Pending;
                }
            }
            ["CANCELPENDING"] => {
                if self.link == Link::Pending {
                    self.link = Link::Idle;
                }
            }
            ["CONNECTED", src, dst, profile @ ..] => {
                self.link = Link::Connected {
                    src: src.to_string(),
                    dst: dst.to_string(),
                    profile: profile.first().unwrap_or(&"").to_string(),
                }
            }
            ["DISCONNECTED"] => {
                self.link = Link::Idle;
                self.level = None;
                self.snr = None;
                self.buffer = 0;
            }
            ["BITRATE", level, bps, ..] => {
                let level = level.trim_matches(|c| c == '(' || c == ')').parse();
                if let (Ok(level), Ok(bps)) = (level, bps.parse()) {
                    self.level = Some((level, bps));
                }
            }
            ["SN", db] => self.snr = db.parse().ok(),
            ["BUFFER", n] => self.buffer = n.parse().unwrap_or(self.buffer),
            ["BUSY", on] => self.busy = *on == "ON",
            ["PTT", on] => self.ptt = *on == "ON",
            _ => {}
        }
    }

    /// The station at the other end of the link.
    pub fn peer(&self) -> Option<&str> {
        let Link::Connected { src, dst, .. } = &self.link else {
            return None;
        };
        let mine = |c: &str| {
            self.mycall
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case(c))
        };
        Some(if mine(src) { dst } else { src })
    }

    /// Receive level in dBFS, smoothed.
    pub fn rx_dbfs(&self) -> f32 {
        dbfs(self.rx)
    }
}

/// A logged host line in words for the window; anything else as it came.
pub fn describe(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["PENDING"] => "Connecting".into(),
        ["CANCELPENDING"] => "The connection attempt ended".into(),
        ["CONNECTED", src, dst, profile @ ..] => format!(
            "Connected: {src} called {dst}{}",
            profile
                .first()
                .map(|p| format!(", {} profile", p.to_lowercase()))
                .unwrap_or_default()
        ),
        ["DISCONNECTED"] => "Disconnected".into(),
        ["BITRATE", level, bps, ..] => format!(
            "Speed level {}, {bps} bps",
            level.trim_matches(|c| c == '(' || c == ')')
        ),
        ["MISSING", "SOUNDCARD"] => "No sound card".into(),
        _ => line.to_string(),
    }
}

/// Below this the channel is quiet or the radio's audio is turned down too far.
pub const QUIET_BELOW_DBFS: f32 = -40.0;
/// Above this the receive audio is close to clipping.
pub const HOT_ABOVE_DBFS: f32 = -6.0;

/// How the receive level reads for setting up a radio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loudness {
    Quiet,
    Good,
    Hot,
}

pub fn loudness(dbfs: f32) -> Loudness {
    if dbfs < QUIET_BELOW_DBFS {
        Loudness::Quiet
    } else if dbfs > HOT_ABOVE_DBFS {
        Loudness::Hot
    } else {
        Loudness::Good
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(lines: &[&str]) -> Status {
        let mut s = Status::default();
        for l in lines {
            s.apply(0, Monitor::Line(l.to_string()));
        }
        s
    }

    #[test]
    fn follows_a_session_the_way_pat_sees_it() {
        let s = fed(&[
            "OK",
            "REGISTERED N0CALL",
            "PENDING",
            "CONNECTED N0CALL W1AW-10 NARROW",
            "BITRATE (7) 6444 BPS",
            "SN 21.5",
            "BUFFER 4096",
            "BUSY ON",
            "PTT ON",
        ]);
        assert_eq!(s.peer(), Some("W1AW-10"));
        assert_eq!(s.level, Some((7, 6444)));
        assert_eq!(s.snr, Some(21.5));
        assert_eq!(s.buffer, 4096);
        assert!(s.busy && s.ptt);
        let s = fed(&[
            "CONNECTED N0CALL W1AW-10 NARROW",
            "BITRATE (7) 6444 BPS",
            "BUFFER 10",
            "DISCONNECTED",
            "PTT OFF",
        ]);
        assert_eq!(s.link, Link::Idle);
        assert_eq!((s.level, s.snr, s.buffer, s.ptt), (None, None, 0, false));
    }

    #[test]
    fn the_peer_is_whichever_call_is_not_ours() {
        // a caller is listed first, so on an incoming link our call is the second one
        let s = fed(&["REGISTERED w1aw-10", "CONNECTED N0CALL W1AW-10 WIDE"]);
        assert_eq!(s.peer(), Some("N0CALL"));
        let Link::Connected { profile, .. } = &s.link else {
            panic!("not connected");
        };
        assert_eq!(profile, "WIDE");
    }

    #[test]
    fn the_client_is_forgotten_when_it_disconnects() {
        let mut s = fed(&["REGISTERED N0CALL"]);
        s.apply(0, Monitor::ClientLeft);
        assert_eq!(s.mycall, None);
    }

    #[test]
    fn a_failed_call_goes_back_to_idle_but_a_link_is_not_undone_by_a_late_cancel() {
        assert_eq!(fed(&["PENDING", "CANCELPENDING"]).link, Link::Idle);
        let s = fed(&["CONNECTED A B NARROW", "CANCELPENDING"]);
        assert!(matches!(s.link, Link::Connected { .. }));
    }

    #[test]
    fn junk_lines_change_nothing() {
        let s = fed(&[
            "BITRATE (x) y BPS",
            "SN loud",
            "BUFFER 12",
            "BUFFER many",
            "FROB",
        ]);
        assert_eq!((s.level, s.snr, s.buffer), (None, None, 12));
    }

    #[test]
    fn host_lines_read_as_words_in_the_log() {
        assert_eq!(describe("PENDING"), "Connecting");
        assert_eq!(
            describe("CONNECTED N0CALL W1AW-10 NARROW"),
            "Connected: N0CALL called W1AW-10, narrow profile"
        );
        assert_eq!(describe("BITRATE (9) 8055 BPS"), "Speed level 9, 8055 bps");
        let note = "no audio from the input device for 3 s";
        assert_eq!(describe(note), note);
        let mut s = Status::default();
        s.apply(0, Monitor::Log("DISCONNECTED".into()));
        assert_eq!(s.log[0].1, "Disconnected");
    }

    #[test]
    fn the_log_keeps_only_the_latest_lines() {
        let mut s = Status::default();
        for i in 0..LOG_LINES + 5 {
            s.apply(i as u64, Monitor::Log(format!("line {i}")));
        }
        assert_eq!(s.log.len(), LOG_LINES);
        assert_eq!(s.log.front().unwrap().1, "line 5");
    }

    #[test]
    fn the_receive_level_settles_on_what_comes_in() {
        let mut s = Status::default();
        for _ in 0..40 {
            s.apply(0, Monitor::Level(0.1));
        }
        assert!((s.rx_dbfs() + 20.0).abs() < 0.1, "{}", s.rx_dbfs());
        assert_eq!(loudness(s.rx_dbfs()), Loudness::Good);
        assert_eq!(loudness(-55.0), Loudness::Quiet);
        assert_eq!(loudness(-2.0), Loudness::Hot);
    }
}
