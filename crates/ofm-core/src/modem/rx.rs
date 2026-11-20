use super::qam::constellation;
use super::{
    bits_to_bytes, interleave, pilot_value, scramble, Chunk, Header, Level, Modulation, Profile,
    FS, LEVELS, MAX_CHUNKS, NCP, NFFT, SYM,
};
use crate::ldpc::{Code, Length, Rate};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::f32::consts::TAU;
use std::sync::Arc;

// fft windows start this far into the cyclic prefix, leaving room for timing error either way
const BACKOFF: usize = 24;
const DETECT: f32 = 0.6;
const MIN_POWER: f32 = 1e-7;
// extra samples kept after a frame's nominal end for clock drift
const MARGIN: usize = 128;
const MAX_ITER: usize = 50;
// real preambles hold the sync metric above DETECT for under 900 samples; a tone goes on forever
const MAX_PLATEAU: usize = 2 * NFFT;
// normalized correlation with the sync symbol: preambles measured 0.13 to 0.96, tones under 0.05
const MIN_SCORE: f32 = 0.08;

/// A frame whose header decoded. Chunks that failed LDPC or CRC are None.
#[derive(Clone, Debug)]
pub struct RxFrame {
    pub header: Header,
    pub chunks: Vec<Option<Chunk>>,
    /// Average per-carrier SNR over the frame.
    pub snr_db: f32,
}

#[derive(Clone, Debug)]
pub enum RxEvent {
    Frame(RxFrame),
    /// A preamble was heard but its header did not decode.
    HeaderError {
        snr_db: f32,
    },
}

enum State {
    Search,
    Synced(usize),
    Frame { t_a: usize, header: Header },
}

struct Demod {
    // equalized data-carrier points and their noise variance, per symbol
    points: Vec<Vec<Complex32>>,
    vars: Vec<Vec<f32>>,
    snr_db: f32,
}

// second-order section, rbj cookbook form
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn new(f0: f32, high: bool) -> Biquad {
        let w = TAU * f0 / FS as f32;
        let alpha = w.sin() / std::f32::consts::SQRT_2;
        let c = w.cos();
        let a0 = 1.0 + alpha;
        let b = if high {
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0]
        } else {
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0]
        };
        Biquad {
            b: b.map(|x| x / a0),
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Streaming receiver for one profile. Feed it audio, get decoded frames back.
pub struct Receiver {
    profile: Profile,
    // keeps out-of-band noise out of the sync metric; the equalizer absorbs its phase
    filter: [Biquad; 2],
    fwd: Arc<dyn Fft<f32>>,
    sync: Vec<f32>,
    sync_values: Vec<Complex32>,
    buf: Vec<f32>,
    pos: usize,
    state: State,
    level: f32,
    floor: f32,
    since_preamble: usize,
}

impl Receiver {
    pub fn new(profile: Profile) -> Receiver {
        let synth = super::tx::Synth::new(profile);
        let sync_values = profile.sync_values();
        let (lo, hi) = match profile {
            Profile::Narrow => (300.0, 3500.0),
            Profile::Wide => (150.0, 5500.0),
        };
        Receiver {
            profile,
            filter: [Biquad::new(lo, true), Biquad::new(hi, false)],
            fwd: FftPlanner::new().plan_fft_forward(NFFT),
            sync: synth.body(&sync_values),
            sync_values,
            buf: Vec::new(),
            pos: 0,
            state: State::Search,
            level: 0.0,
            floor: f32::MAX,
            since_preamble: usize::MAX / 2,
        }
    }

    /// Drops buffered audio, e.g. after our own transmission.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.pos = 0;
        self.state = State::Search;
    }

    /// True between a detected preamble and the end of its frame.
    pub fn receiving(&self) -> bool {
        !matches!(self.state, State::Search)
    }

    /// True while a frame is being received, a preamble was heard in the last second,
    /// or in-band energy is well above the noise floor.
    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Search) || self.since_preamble < FS || self.energy()
    }

    /// True while in-band audio is well above the noise floor.
    pub fn energy(&self) -> bool {
        self.level > MIN_POWER && self.level > 4.0 * self.floor
    }

    pub fn push(&mut self, samples: &[f32]) -> Vec<RxEvent> {
        let [hp, lp] = &mut self.filter;
        let filtered: Vec<f32> = samples.iter().map(|&x| lp.run(hp.run(x))).collect();
        self.track_level(&filtered);
        self.buf.extend_from_slice(&filtered);
        let mut events = Vec::new();
        loop {
            match std::mem::replace(&mut self.state, State::Search) {
                State::Search => match self.detect() {
                    Some(t_a) => {
                        self.since_preamble = 0;
                        self.state = State::Synced(t_a);
                    }
                    None => break,
                },
                State::Synced(t_a) => {
                    let hs = self.profile.header_symbols();
                    if self.buf.len() < t_a + 2 * NFFT + hs * SYM + MARGIN {
                        self.state = State::Synced(t_a);
                        break;
                    }
                    match self.decode_header(t_a) {
                        Ok(header) => self.state = State::Frame { t_a, header },
                        // search resumes after the plateau, which detect() already set
                        Err(snr_db) => events.push(RxEvent::HeaderError { snr_db }),
                    }
                }
                State::Frame { t_a, header } => {
                    let level = LEVELS[header.level as usize];
                    let nsyms = self.profile.header_symbols()
                        + level.symbols(self.profile, header.count as usize);
                    let end = t_a + 2 * NFFT + nsyms * SYM;
                    if self.buf.len() < end + MARGIN {
                        self.state = State::Frame { t_a, header };
                        break;
                    }
                    events.push(RxEvent::Frame(
                        self.decode_frame(t_a, header, &level, nsyms),
                    ));
                    self.pos = end;
                }
            }
        }
        self.trim();
        events
    }

    fn track_level(&mut self, samples: &[f32]) {
        for block in samples.chunks(480) {
            let p = block.iter().map(|x| x * x).sum::<f32>() / block.len() as f32;
            self.level = 0.5 * self.level + 0.5 * p;
            // the floor follows quiet stretches down at once and creeps up slowly; it never
            // reaches zero, or it could not creep up again after digital silence
            self.floor = if self.level < self.floor {
                self.level.max(MIN_POWER / 4.0)
            } else {
                self.floor * 1.002
            };
        }
        self.since_preamble = self.since_preamble.saturating_add(samples.len());
    }

    fn trim(&mut self) {
        let keep_from = match self.state {
            State::Search => self.pos,
            State::Synced(t) | State::Frame { t_a: t, .. } => t,
        }
        .saturating_sub(NFFT);
        if keep_from == 0 {
            return;
        }
        self.buf.drain(..keep_from);
        self.pos = self.pos.saturating_sub(keep_from);
        self.state = match std::mem::replace(&mut self.state, State::Search) {
            State::Search => State::Search,
            State::Synced(t) => State::Synced(t - keep_from),
            State::Frame { t_a, header } => State::Frame {
                t_a: t_a - keep_from,
                header,
            },
        };
    }

    // schmidl-cox style search for the repeated sync symbol, then cross-correlation for timing.
    // returns the start of the first sync copy (after its prefix).
    fn detect(&mut self) -> Option<usize> {
        let n = self.buf.len();
        let l = NFFT;
        let start = self.pos.max(BACKOFF);
        if n < start + 2 * l + 2 * NCP {
            return None;
        }
        let last = n - 2 * l - NCP;
        let r = &self.buf;
        let mut corr = vec![0f64; last - start + l + 1];
        for i in 0..last - start + l {
            corr[i + 1] = corr[i] + (r[start + i] as f64) * (r[start + i + l] as f64);
        }
        let mut energy = vec![0f64; last - start + 2 * l + 1];
        for i in 0..last - start + 2 * l {
            energy[i + 1] = energy[i] + (r[start + i] as f64).powi(2);
        }
        let metric = |d: usize| -> f32 {
            let i = d - start;
            let e = 0.5 * (energy[i + 2 * l] - energy[i]);
            if e / (l as f64) < MIN_POWER as f64 {
                return 0.0;
            }
            ((corr[i + l] - corr[i]) / e) as f32
        };

        let mut d = start;
        while d < last {
            if metric(d) < DETECT {
                d += 1;
                continue;
            }
            let region_start = d;
            let mut best = (metric(d), d);
            while d < last && metric(d) >= DETECT {
                let m = metric(d);
                if m > best.0 {
                    best = (m, d);
                }
                d += 1;
            }
            if d >= last {
                // plateau may continue past what we have; look again once more audio arrives,
                // unless it is already longer than any preamble (a steady tone or hum)
                self.pos = if last - region_start > MAX_PLATEAU {
                    last
                } else {
                    region_start
                };
                return None;
            }
            // a real preamble holds the metric up for at least the prefix length; short blips
            // near the threshold are noise or the edge of other audio
            if !(NCP / 2..=MAX_PLATEAU).contains(&(d - region_start)) || best.0 < 0.75 {
                continue;
            }
            let near: Vec<usize> = (region_start..d)
                .filter(|&x| metric(x) >= best.0 - 0.05)
                .collect();
            let centre = (near[0] + near[near.len() - 1]) / 2;
            let coarse = centre + NCP / 2;
            let t_a = self.fine_timing(coarse);
            if self.best_score(coarse) < MIN_SCORE {
                continue;
            }
            self.pos = d;
            return Some(t_a);
        }
        self.pos = last;
        None
    }

    // a tilted or band-limited path smears the correlation peak well past the fine timing
    // window, so the gate looks wider than that window does
    fn best_score(&self, coarse: usize) -> f32 {
        let lo = coarse.saturating_sub(256);
        let hi = (coarse + 256).min(self.buf.len() - NFFT);
        let s: f32 = self.sync.iter().map(|x| x * x).sum();
        (lo..=hi)
            .map(|t| {
                let w = &self.buf[t..t + NFFT];
                let c: f32 = w.iter().zip(&self.sync).map(|(a, b)| a * b).sum();
                let e: f32 = w.iter().map(|x| x * x).sum();
                c * c / (e * s).max(1e-20)
            })
            .fold(0.0, f32::max)
    }

    fn fine_timing(&self, coarse: usize) -> usize {
        let lo = coarse.saturating_sub(64).max(BACKOFF);
        let hi = (coarse + 64).min(self.buf.len() - NFFT);
        (lo..=hi)
            .map(|t| {
                let c: f32 = self.buf[t..t + NFFT]
                    .iter()
                    .zip(&self.sync)
                    .map(|(a, b)| a * b)
                    .sum();
                (c, t)
            })
            .fold((f32::MIN, coarse), |a, b| if b.0 > a.0 { b } else { a })
            .1
    }

    fn spectrum(&self, start: usize) -> Vec<Complex32> {
        let mut x: Vec<Complex32> = self.buf[start..start + NFFT]
            .iter()
            .map(|&v| Complex32::new(v, 0.0))
            .collect();
        self.fwd.process(&mut x);
        (0..self.profile.carriers())
            .map(|c| x[self.profile.bin(c)])
            .collect()
    }

    fn demod(&self, t_a: usize, nsyms: usize) -> Demod {
        let p = self.profile;
        let y1 = self.spectrum(t_a - BACKOFF);
        let y2 = self.spectrum(t_a + NFFT - BACKOFF);
        let h: Vec<Complex32> = (0..p.carriers())
            .map(|c| (y1[c] + y2[c]) * 0.5 / self.sync_values[c])
            .collect();
        let pre_var = y1
            .iter()
            .zip(&y2)
            .map(|(a, b)| (a - b).norm_sqr())
            .sum::<f32>()
            / (2 * p.carriers()) as f32;
        let signal = h.iter().map(|x| x.norm_sqr()).sum::<f32>() / h.len() as f32;

        let pilots = p.pilot_carriers();
        let data = p.data_carriers();
        let mut var = pre_var.max(signal * 1e-6);
        let mut var_sum = 0.0;
        let mut shift: isize = 0;
        let mut out = Demod {
            points: Vec::with_capacity(nsyms),
            vars: Vec::with_capacity(nsyms),
            snr_db: 0.0,
        };

        for s in 0..nsyms {
            let nominal = t_a + 2 * NFFT + NCP + s * SYM - BACKOFF;
            let y = self.spectrum((nominal as isize + shift) as usize);
            let z: Vec<Complex32> = pilots
                .iter()
                .map(|&c| y[c] / (h[c] * pilot_value(s, c)))
                .collect();

            // phase slope across carriers is timing error; common phase and gain ride on top
            let (mut num, mut den) = (0.0f32, 0.0f32);
            for i in 1..pilots.len() {
                let gap = (pilots[i] - pilots[i - 1]) as f32;
                let w = gap * z[i].norm() * z[i - 1].norm();
                num += w * (z[i] * z[i - 1].conj()).arg() / gap;
                den += w;
            }
            let slope = if den > 0.0 { num / den } else { 0.0 };
            let common: Complex32 = pilots
                .iter()
                .zip(&z)
                .map(|(&c, zi)| zi * Complex32::from_polar(1.0, -slope * c as f32))
                .sum();
            let gain = z.iter().map(|zi| zi.norm()).sum::<f32>() / z.len() as f32;
            let phase = common.arg();
            let fix = |c: usize| h[c] * Complex32::from_polar(gain, phase + slope * c as f32);

            let resid: f32 = pilots
                .iter()
                .map(|&c| (y[c] - fix(c) * pilot_value(s, c)).norm_sqr())
                .sum::<f32>()
                / (pilots.len() as f32 - 1.5);
            var = 0.8 * var + 0.2 * resid;
            var_sum += var;

            out.points
                .push(data.iter().map(|&c| y[c] / fix(c)).collect());
            out.vars
                .push(data.iter().map(|&c| var / fix(c).norm_sqr()).collect());

            let tau = -slope * NFFT as f32 / TAU;
            if tau.abs() > 3.0 {
                shift = (shift + tau.round() as isize).clamp(-60, 60);
            }
        }
        let mean_var = if nsyms > 0 {
            var_sum / nsyms as f32
        } else {
            pre_var
        };
        out.snr_db = 10.0 * (signal / mean_var.max(1e-20)).log10();
        out
    }

    fn llrs(demod: &Demod, from: usize, modulation: Modulation) -> Vec<f32> {
        let con = constellation(modulation);
        let mut out = Vec::new();
        for (s, (pts, vars)) in demod.points.iter().zip(&demod.vars).enumerate().skip(from) {
            let start = out.len();
            for (&y, &v) in pts.iter().zip(vars) {
                con.demap(y, v, &mut out);
            }
            for (slot, llr) in out[start..].iter_mut().enumerate() {
                if scramble(s, slot) == 1 {
                    *llr = -*llr;
                }
            }
        }
        out
    }

    fn decode_header(&self, t_a: usize) -> Result<Header, f32> {
        let hs = self.profile.header_symbols();
        let demod = self.demod(t_a, hs);
        let slots = Self::llrs(&demod, 0, Modulation::Bpsk);
        let code = Code::new(Length::N648, Rate::R1_2);
        let llr: Vec<f32> = (0..648).map(|i| slots[interleave(i, 648, 0)]).collect();
        code.decode(&llr, MAX_ITER)
            .and_then(|bits| Header::from_bytes(&bits_to_bytes(&bits)))
            .filter(|h| {
                h.profile == self.profile
                    && (h.level as usize) < LEVELS.len()
                    && h.count as usize <= MAX_CHUNKS
            })
            .ok_or(demod.snr_db)
    }

    fn decode_frame(&self, t_a: usize, header: Header, level: &Level, nsyms: usize) -> RxFrame {
        let demod = self.demod(t_a, nsyms);
        let slots = Self::llrs(&demod, self.profile.header_symbols(), level.modulation);
        let code = Code::new(level.len, level.rate);
        let n = level.n();
        let chunks = (0..header.count as usize)
            .map(|j| {
                let base = j * n * level.rep;
                let llr: Vec<f32> = (0..n)
                    .map(|i| {
                        (0..level.rep)
                            .map(|cp| slots[base + cp * n + interleave(i, n, cp)])
                            .sum()
                    })
                    .collect();
                code.decode(&llr, MAX_ITER)
                    .and_then(|bits| Chunk::from_bytes(&bits_to_bytes(&bits)))
            })
            .collect();
        RxFrame {
            header,
            chunks,
            snr_db: demod.snr_db,
        }
    }
}
