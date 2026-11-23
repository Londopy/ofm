// level calibration: a steady tone at the modem's own peak level so deviation can be set
// without a service monitor, plus a live receive level meter for the other station's audio

use crate::audio::SoundCard;
use crate::config::Config;
use crate::ptt::Ptt;
use crate::station::Audio;
use anyhow::Result;
use ofm_core::modem::{build_frame, Chunk, Header, Kind, Profile, FS, LEVELS};
use std::io::{BufRead, Write};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

const TONE_HZ: f32 = 1500.0;
// keep a stuck transmitter from cooking the radio
pub const MAX_TX: Duration = Duration::from_secs(30);
const BLOCK: Duration = Duration::from_millis(100);
// tone queued ahead of the sound card, so a 100 ms poll never leaves a gap
const AHEAD: Duration = Duration::from_millis(300);
/// One step of the transmit gain controls.
pub const GAIN_STEP_DB: f32 = 1.0;

// peak of a real top-level frame, so the tone deviates the radio as far as data will
fn modem_peak(profile: Profile, papr_db: f32) -> f32 {
    let level = &LEVELS[LEVELS.len() - 1];
    let cap = Chunk::capacity(level.info_bytes());
    let chunks: Vec<Chunk> = (0..8)
        .map(|i| Chunk {
            offset: (i * cap) as u32,
            data: (0..cap).map(|j| (i * 7 + j * 13) as u8).collect(),
        })
        .collect();
    let mut h = Header::new(Kind::Data, profile, "CAL", "CAL");
    h.level = (LEVELS.len() - 1) as u8;
    build_frame(&h, &chunks, papr_db)
        .iter()
        .fold(0.0f32, |m, x| m.max(x.abs()))
}

pub fn dbfs(x: f32) -> f32 {
    20.0 * x.max(1e-6).log10()
}

/// `n` samples of the calibration tone at amplitude `amp`, starting `phase` samples in.
pub fn tone(amp: f32, phase: usize, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| amp * (std::f32::consts::TAU * TONE_HZ * (phase + i) as f32 / FS as f32).sin())
        .collect()
}

/// Gain after `steps` presses of + (or - when negative).
pub fn step_gain(gain: f32, steps: i32) -> f32 {
    gain * 10f32.powf(steps as f32 * GAIN_STEP_DB / 20.0)
}

/// A sound card and PTT held for calibrating: keys the tone, keeps it fed, and measures what
/// comes in. Dropping it releases PTT.
pub struct Calibrator {
    card: SoundCard,
    ptt: Box<dyn Ptt>,
    peak: f32,
    keyed_at: Option<Instant>,
    tone_end: Instant,
    phase: usize,
    level: f32,
}

impl Calibrator {
    pub fn open(cfg: &Config, profile: Profile, ptt: Box<dyn Ptt>) -> Result<Calibrator> {
        let card = SoundCard::open(cfg.audio.input.as_deref(), cfg.audio.output.as_deref())?;
        Ok(Calibrator {
            card,
            ptt,
            peak: modem_peak(profile, cfg.radio.papr_db),
            keyed_at: None,
            tone_end: Instant::now(),
            phase: 0,
            level: 0.0,
        })
    }

    pub fn keyed(&self) -> bool {
        self.keyed_at.is_some()
    }

    pub fn set_tone(&mut self, on: bool) -> Result<()> {
        if on == self.keyed() {
            return Ok(());
        }
        self.ptt.set(on)?;
        self.keyed_at = on.then(Instant::now);
        self.tone_end = Instant::now();
        Ok(())
    }

    /// Keeps the tone going at `gain` and reads the receive level. Call it every 100 ms or
    /// so. Returns true when it has just unkeyed after MAX_TX.
    pub fn poll(&mut self, gain: f32) -> Result<bool> {
        let mut timed_out = false;
        if self.keyed_at.is_some_and(|t| t.elapsed() > MAX_TX) {
            self.set_tone(false)?;
            timed_out = true;
        }
        let amp = (self.peak * gain).min(1.0);
        let n = FS * BLOCK.as_millis() as usize / 1000;
        while self.keyed() && self.tone_end < Instant::now() + AHEAD {
            self.card.play(tone(amp, self.phase, n));
            self.phase += n;
            self.tone_end += BLOCK;
        }
        let heard = self.card.capture();
        if !heard.is_empty() {
            let rms = (heard.iter().map(|x| x * x).sum::<f32>() / heard.len() as f32).sqrt();
            self.level = 0.7 * self.level + 0.3 * rms;
        }
        Ok(timed_out)
    }

    /// Receive level in dBFS.
    pub fn level_db(&self) -> f32 {
        dbfs(self.level)
    }
}

impl Drop for Calibrator {
    fn drop(&mut self) {
        if self.keyed() {
            if let Err(e) = self.ptt.set(false) {
                eprintln!("ptt off failed: {e}");
            }
        }
    }
}

pub fn run(cfg: &mut Config, profile: Profile, ptt: Box<dyn Ptt>) -> Result<()> {
    let mut cal = Calibrator::open(cfg, profile, ptt)?;
    println!("t = transmit tone on/off, + / - = tx gain 1 dB, s = save, q = quit (then Enter)");
    println!(
        "set the radio's deviation with the tone keyed: about 3 kHz on narrow, 3.5 kHz on wide"
    );

    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        if crate::stop::requested() {
            println!();
            return Ok(());
        }
        while let Ok(line) = lines.try_recv() {
            match line.trim() {
                "t" => {
                    let on = !cal.keyed();
                    cal.set_tone(on)?;
                }
                "+" => cfg.audio.tx_gain = step_gain(cfg.audio.tx_gain, 1),
                "-" => cfg.audio.tx_gain = step_gain(cfg.audio.tx_gain, -1),
                "s" => {
                    cfg.save()?;
                    println!("\nsaved tx gain {:.1} dB", dbfs(cfg.audio.tx_gain));
                }
                "q" => {
                    println!();
                    return Ok(());
                }
                _ => {}
            }
        }
        if cal.poll(cfg.audio.tx_gain)? {
            println!("\ntone stopped after {} s", MAX_TX.as_secs());
        }
        let db = cal.level_db();
        let bars = ((db + 60.0) / 2.0).clamp(0.0, 30.0) as usize;
        print!(
            "\rrx {:6.1} dBFS [{:<30}]  tx gain {:5.1} dB {}   ",
            db,
            "#".repeat(bars),
            dbfs(cfg.audio.tx_gain),
            if cal.keyed() { "TX" } else { "  " }
        );
        std::io::stdout().flush()?;
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_blocks_join_without_a_step() {
        let whole = tone(0.5, 0, 9600);
        let mut pieces = tone(0.5, 0, 4800);
        pieces.extend(tone(0.5, 4800, 4800));
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-4);
        }
        let peak = whole.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn gain_steps_are_one_db_and_undo_each_other() {
        let g = step_gain(1.0, 3);
        assert!((dbfs(g) - 3.0).abs() < 1e-3);
        assert!((step_gain(g, -3) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn the_tone_peaks_where_a_top_level_frame_does() {
        // over-deviation is the usual failure, so the tone must not sit below real peaks
        for p in [Profile::Narrow, Profile::Wide] {
            let peak = modem_peak(p, 9.0);
            assert!((0.45..0.7).contains(&peak), "{p:?} peak {peak}");
        }
    }
}
