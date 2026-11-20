//! FM audio channel simulator: what one radio's sound card port hears from another's.

use std::f64::consts::{LOG10_2, PI, TAU};

/// Impairments of one direction of an FM audio link.
#[derive(Clone, Debug)]
pub struct ChannelConfig {
    /// Audio sample rate in Hz.
    pub sample_rate: f32,
    /// AWGN as band-passed signal power over total white-noise power, in dB; None adds no noise.
    pub snr_db: Option<f32>,
    /// Residual pre/de-emphasis mismatch in dB per octave above `tilt_corner_hz`.
    /// Positive boosts highs, 0 is flat.
    pub tilt_db_per_octave: f32,
    /// Corner of the tilt in Hz.
    pub tilt_corner_hz: f32,
    /// Audio passband edges in Hz, each a 4th-order Butterworth.
    pub bandpass: Option<(f32, f32)>,
    /// TX deviation limiter: hard clip at this fraction of the input peak, ahead of all filtering.
    pub clip: Option<f32>,
    /// RX sample clock offset vs TX in ppm. Positive is a fast RX clock: the output has
    /// round(len * (1 + ppm * 1e-6)) samples and tones come out lower by that ratio.
    pub clock_ppm: f32,
    /// Audio lost to keyup and squelch at the start of each transmission in ms, then a thump.
    pub keyup_ms: f32,
    /// Noise generator seed.
    pub seed: u64,
}

impl ChannelConfig {
    /// Narrow profile path: mic/speaker or 1200-baud data jack.
    pub fn narrow() -> Self {
        Self {
            sample_rate: 48000.0,
            snr_db: None,
            tilt_db_per_octave: 1.5,
            tilt_corner_hz: 300.0,
            bandpass: Some((300.0, 3000.0)),
            clip: None,
            clock_ppm: 0.0,
            keyup_ms: 0.0,
            seed: 1,
        }
    }

    /// Wide profile path: flat 9600 discriminator port.
    pub fn wide() -> Self {
        Self {
            tilt_db_per_octave: 0.0,
            bandpass: Some((100.0, 5500.0)),
            ..Self::narrow()
        }
    }
}

/// One direction of an FM link; filter and noise state persist across transmissions.
pub struct Channel {
    cfg: ChannelConfig,
    filters: Vec<Biquad>,
    rng: Rng,
}

impl Channel {
    pub fn new(cfg: ChannelConfig) -> Self {
        let fs = f64::from(cfg.sample_rate);
        let mut filters = Vec::new();
        if cfg.tilt_db_per_octave != 0.0 {
            filters.extend(tilt(
                f64::from(cfg.tilt_db_per_octave),
                f64::from(cfg.tilt_corner_hz),
                fs,
            ));
        }
        if let Some((lo, hi)) = cfg.bandpass {
            filters.extend(butterworth4(f64::from(lo), fs, true));
            filters.extend(butterworth4(f64::from(hi), fs, false));
        }
        let rng = Rng(cfg.seed);
        Self { cfg, filters, rng }
    }

    /// Runs one transmission (one keyup) through the channel. Each call is a separate transmission:
    /// keyup truncation applies at the start of every call, filter state carries over.
    pub fn transmit(&mut self, tx: &[f32]) -> Vec<f32> {
        let fs = f64::from(self.cfg.sample_rate);
        let mut x: Vec<f64> = tx.iter().map(|&v| f64::from(v)).collect();
        if let Some(clip) = self.cfg.clip {
            let level = f64::from(clip) * peak(&x);
            x.iter_mut().for_each(|v| *v = v.clamp(-level, level));
        }
        for f in &mut self.filters {
            x.iter_mut().for_each(|v| *v = f.run(*v));
        }
        let mut y = resample(&x, 1.0 + f64::from(self.cfg.clock_ppm) * 1e-6);
        let power = signal_power(&y);
        let gap = (f64::from(self.cfg.keyup_ms) * 1e-3 * fs).round() as usize;
        if gap > 0 {
            keyup(&mut y, gap, 2.0 * power.sqrt(), fs);
        }
        if let Some(snr) = self.cfg.snr_db {
            let sigma = (power / 10f64.powf(f64::from(snr) / 10.0)).sqrt();
            y.iter_mut().for_each(|v| *v += sigma * self.rng.gauss());
        }
        y.into_iter().map(|v| v as f32).collect()
    }
}

#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    s: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b: b.map(|v| v / a[0]),
            a: [a[1] / a[0], a[2] / a[0]],
            s: [0.0; 2],
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.s[0];
        self.s[0] = self.b[1] * x - self.a[0] * y + self.s[1];
        self.s[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

fn butterworth4(fc: f64, fs: f64, high: bool) -> [Biquad; 2] {
    let w = TAU * fc / fs;
    let cos = w.cos();
    let b = if high {
        [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0]
    } else {
        [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0]
    };
    // pole pairs at pi/8 and 3pi/8; q = 1 / (2 cos theta), so sin(w) / 2q = sin(w) cos(theta)
    [PI / 8.0, 3.0 * PI / 8.0].map(|theta| {
        let alpha = w.sin() * theta.cos();
        Biquad::new(b, [1.0 + alpha, -2.0 * cos, 1.0 - alpha])
    })
}

// first-order shelf, unity at dc, zero at fz and pole at fp, bilinear with both prewarped
fn shelf(fz: f64, fp: f64, fs: f64) -> Biquad {
    let tz = (PI * fz / fs).tan();
    let tp = (PI * fp / fs).tan();
    let g = tp / tz;
    Biquad::new(
        [g * (1.0 + tz), -g * (1.0 - tz), 0.0],
        [1.0 + tp, -(1.0 - tp), 0.0],
    )
}

// one first-order shelf only does 6 dB/oct, so other slopes are built from shelf steps two
// octaves apart; at 6 dB/oct they collapse into a single shelf with its zero at the corner
fn tilt(db_per_oct: f64, corner: f64, fs: f64) -> Vec<Biquad> {
    assert!(corner > 0.0, "tilt_corner_hz must be positive");
    let frac = db_per_oct.abs() / (20.0 * LOG10_2);
    let step = 4f64.powf(frac);
    let top = 0.4 * fs;
    let mut lo = corner * 2f64.powf(1.0 - frac);
    let mut sections = Vec::new();
    while lo < top {
        let hi = (lo * step).min(top);
        sections.push(if db_per_oct > 0.0 {
            shelf(lo, hi, fs)
        } else {
            shelf(hi, lo, fs)
        });
        lo *= 4.0;
    }
    sections
}

// output sample n is the input at n / ratio; anything outside the input is silence
fn resample(x: &[f64], ratio: f64) -> Vec<f64> {
    let len = (x.len() as f64 * ratio).round() as usize;
    (0..len)
        .map(|n| {
            let pos = n as f64 / ratio;
            let i = pos.floor() as usize;
            lagrange6(pos - pos.floor())
                .iter()
                .enumerate()
                .map(|(k, w)| {
                    w * (i + k)
                        .checked_sub(2)
                        .and_then(|j| x.get(j))
                        .unwrap_or(&0.0)
                })
                .sum()
        })
        .collect()
}

// weights for samples i-2..=i+3 when interpolating at i + t
fn lagrange6(t: f64) -> [f64; 6] {
    let mut w = [1.0; 6];
    for (k, wk) in w.iter_mut().enumerate() {
        for j in (0..6).filter(|&j| j != k) {
            *wk *= (t + 2.0 - j as f64) / (k as f64 - j as f64);
        }
    }
    w
}

fn peak(x: &[f64]) -> f64 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

// mean square over samples within 60 dB of the peak, so silence around the burst doesn't count
fn signal_power(x: &[f64]) -> f64 {
    let floor = 1e-3 * peak(x);
    let (sum, n) = x
        .iter()
        .filter(|v| v.abs() > floor)
        .fold((0.0, 0usize), |(s, n), v| (s + v * v, n + 1));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

// mutes the keyup gap, then the squelch-open thump: a 100 Hz swing dying out within ~20 ms
fn keyup(y: &mut [f64], gap: usize, amp: f64, fs: f64) {
    let gap = gap.min(y.len());
    y[..gap].fill(0.0);
    for (i, v) in y[gap..].iter_mut().take((0.03 * fs) as usize).enumerate() {
        let t = i as f64 / fs;
        *v += amp * (-t / 0.005).exp() * (TAU * 100.0 * t).sin();
    }
}

// splitmix64
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    // in (0, 1] so the log in gauss stays finite
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 * 2f64.powi(-53)
    }

    fn gauss(&mut self) -> f64 {
        (-2.0 * self.uniform().ln()).sqrt() * (TAU * self.uniform()).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 48000.0;

    fn flat() -> ChannelConfig {
        ChannelConfig {
            tilt_db_per_octave: 0.0,
            bandpass: None,
            ..ChannelConfig::narrow()
        }
    }

    fn tone(freq: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (0.5 * (TAU * freq * i as f64 / FS).sin()) as f32)
            .collect()
    }

    fn power(x: &[f32]) -> f64 {
        x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64
    }

    // steady-state gain, skipping 200 ms of filter transient
    fn gain_db(cfg: ChannelConfig, freq: f64) -> f64 {
        let x = tone(freq, 48000);
        let y = Channel::new(cfg).transmit(&x);
        10.0 * (power(&y[9600..]) / power(&x[9600..])).log10()
    }

    #[test]
    fn narrow_passband() {
        let cfg = ChannelConfig {
            tilt_db_per_octave: 0.0,
            ..ChannelConfig::narrow()
        };
        let pass = gain_db(cfg.clone(), 1000.0);
        assert!(pass.abs() < 1.0, "1000 Hz: {pass} dB");
        for freq in [100.0, 6000.0] {
            let stop = gain_db(cfg.clone(), freq);
            assert!(stop < -15.0, "{freq} Hz: {stop} dB");
        }
    }

    #[test]
    fn tilt_slope() {
        let rise = |db| {
            let cfg = ChannelConfig {
                tilt_db_per_octave: db,
                ..flat()
            };
            gain_db(cfg.clone(), 2400.0) - gain_db(cfg, 600.0)
        };
        let up = rise(6.0);
        assert!((9.0..13.0).contains(&up), "+6 dB/oct: {up} dB");
        let down = rise(-1.5);
        assert!((-4.0..-2.0).contains(&down), "-1.5 dB/oct: {down} dB");
    }

    #[test]
    fn snr_against_burst_power() {
        // one second of tones with half a second of silence either side
        let mut x = vec![0.0f32; 96000];
        for (i, v) in x[24000..72000].iter_mut().enumerate() {
            let t = i as f64 / FS;
            let s: f64 = [500.0, 1100.0, 1700.0, 2300.0, 2900.0]
                .iter()
                .enumerate()
                .map(|(k, f)| 0.2 * (TAU * f * t + k as f64).sin())
                .sum();
            *v = s as f32;
        }
        let clean = Channel::new(ChannelConfig::narrow()).transmit(&x);
        let signal = power(&clean[28800..72000]);
        for snr in [10.0, 20.0] {
            let cfg = ChannelConfig {
                snr_db: Some(snr),
                ..ChannelConfig::narrow()
            };
            let noisy = Channel::new(cfg).transmit(&x);
            let noise: Vec<f32> = noisy.iter().zip(&clean).map(|(a, b)| a - b).collect();
            let measured = 10.0 * (signal / power(&noise)).log10();
            assert!(
                (measured - f64::from(snr)).abs() < 0.5,
                "{snr} dB set, {measured} dB measured"
            );
        }
    }

    // frequency from upward zero crossings, interpolated between samples
    fn frequency(y: &[f32]) -> f64 {
        let t: Vec<f64> = y
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] <= 0.0 && w[1] > 0.0)
            .map(|(i, w)| i as f64 + f64::from(w[0] / (w[0] - w[1])))
            .collect();
        (t.len() - 1) as f64 * FS / (t[t.len() - 1] - t[0])
    }

    #[test]
    fn clock_offset() {
        let x = tone(1000.0, 96000);
        for (ppm, len, freq) in [(1000.0, 96096, 999.001), (-1000.0, 95904, 1001.001)] {
            let cfg = ChannelConfig {
                clock_ppm: ppm,
                ..flat()
            };
            let y = Channel::new(cfg).transmit(&x);
            assert_eq!(y.len(), len);
            let f = frequency(&y[100..len - 100]);
            assert!((f - freq).abs() < 0.01, "{ppm} ppm: {f} Hz");
        }
    }

    #[test]
    fn clip_limits_peak() {
        let x: Vec<f32> = (0..9600)
            .map(|i| {
                let t = i as f64 / FS;
                (0.6 * (TAU * 700.0 * t).sin() + 0.4 * (TAU * 1900.0 * t).sin()) as f32
            })
            .collect();
        let level = 0.7 * x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let cfg = ChannelConfig {
            clip: Some(0.7),
            ..flat()
        };
        let y = Channel::new(cfg).transmit(&x);
        assert!(y.iter().all(|v| v.abs() <= level + 1e-6));
        assert!(y.iter().any(|v| v.abs() >= level - 1e-6));
        for (a, b) in x.iter().zip(&y) {
            if a.abs() < level {
                assert!((a - b).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn keyup_mutes_each_transmission() {
        let x = tone(1000.0, 24000);
        let mut ch = Channel::new(ChannelConfig {
            keyup_ms: 50.0,
            ..flat()
        });
        for _ in 0..2 {
            let y = ch.transmit(&x);
            assert!(y[..2400].iter().all(|v| v.abs() < 1e-6));
            let after = 10.0 * (power(&y[2400..]) / power(&x[2400..])).log10();
            assert!(after.abs() < 1.0, "{after} dB after keyup");
            let thump: Vec<f32> = y[2400..2880]
                .iter()
                .zip(&x[2400..])
                .map(|(a, b)| a - b)
                .collect();
            assert!(power(&thump) > 0.01 * power(&x));
        }
    }
}
