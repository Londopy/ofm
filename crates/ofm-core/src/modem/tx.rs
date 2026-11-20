use super::qam::constellation;
use super::{
    bytes_to_bits, interleave, pilot_value, pn, scramble, Chunk, Header, Level, Modulation,
    Profile, FS, LEVELS, NCP, NFFT, PREAMBLE, SYM,
};
use crate::ldpc::{Code, Length, Rate};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

// frame rms. clip-and-filter lets peaks regrow to about 12 dB over rms, so this keeps them
// near 0.65 of full scale; sound card paths that limit or clip at 1.0 wreck the signal
pub(crate) const TARGET_RMS: f32 = 0.16;

pub(crate) struct Synth {
    profile: Profile,
    inv: Arc<dyn Fft<f32>>,
    fwd: Arc<dyn Fft<f32>>,
}

impl Synth {
    pub fn new(profile: Profile) -> Synth {
        let mut planner = FftPlanner::new();
        Synth {
            profile,
            inv: planner.plan_fft_inverse(NFFT),
            fwd: planner.plan_fft_forward(NFFT),
        }
    }

    pub fn body(&self, values: &[Complex32]) -> Vec<f32> {
        let mut x = vec![Complex32::default(); NFFT];
        for (c, &v) in values.iter().enumerate() {
            let b = self.profile.bin(c);
            x[b] = v;
            x[NFFT - b] = v.conj();
        }
        self.inv.process(&mut x);
        x.iter().map(|z| z.re / NFFT as f32).collect()
    }

    // clip peaks, then filter the clipping products back out of band; two passes catch regrowth
    fn limit(&self, body: &mut [f32], papr_db: f32) {
        let used: Vec<usize> = (0..self.profile.carriers())
            .map(|c| self.profile.bin(c))
            .collect();
        for _ in 0..2 {
            let rms = (body.iter().map(|x| x * x).sum::<f32>() / NFFT as f32).sqrt();
            let peak = rms * 10f32.powf(papr_db / 20.0);
            let mut x: Vec<Complex32> = body
                .iter()
                .map(|&v| Complex32::new(v.clamp(-peak, peak), 0.0))
                .collect();
            self.fwd.process(&mut x);
            let mut y = vec![Complex32::default(); NFFT];
            for &b in &used {
                y[b] = x[b];
                y[NFFT - b] = x[NFFT - b];
            }
            self.inv.process(&mut y);
            for (o, z) in body.iter_mut().zip(&y) {
                *o = z.re / NFFT as f32;
            }
        }
    }

    pub fn symbol(&self, values: &[Complex32], papr_db: f32) -> Vec<f32> {
        let mut body = self.body(values);
        self.limit(&mut body, papr_db);
        let mut out = Vec::with_capacity(SYM);
        out.extend_from_slice(&body[NFFT - NCP..]);
        out.extend_from_slice(&body);
        out
    }
}

fn coded(code: &Code, bytes: &[u8], rep: usize) -> Vec<u8> {
    let cw = code.encode(&bytes_to_bits(bytes, code.k()));
    let n = cw.len();
    let mut out = vec![0u8; n * rep];
    for copy in 0..rep {
        for (i, &bit) in cw.iter().enumerate() {
            out[copy * n + interleave(i, n, copy)] = bit;
        }
    }
    out
}

fn emit(
    synth: &Synth,
    modulation: Modulation,
    bits: &[u8],
    symbol: &mut usize,
    papr_db: f32,
    out: &mut Vec<f32>,
) {
    let profile = synth.profile;
    let data = profile.data_carriers();
    let m = modulation.bits();
    let per = data.len() * m;
    let con = constellation(modulation);
    let mut values = vec![Complex32::default(); profile.carriers()];
    for start in (0..bits.len()).step_by(per) {
        for c in profile.pilot_carriers() {
            values[c] = Complex32::new(pilot_value(*symbol, c), 0.0);
        }
        let mut slot = [0u8; 6];
        for (j, &c) in data.iter().enumerate() {
            for (b, s) in slot[..m].iter_mut().enumerate() {
                let idx = start + j * m + b;
                let bit = bits
                    .get(idx)
                    .copied()
                    .unwrap_or(u8::from(pn(*symbol as u32, idx as u32)));
                *s = bit ^ scramble(*symbol, j * m + b);
            }
            values[c] = con.map(&slot[..m]);
        }
        out.extend(synth.symbol(&values, papr_db));
        *symbol += 1;
    }
}

/// Length in samples of a frame with `count` chunks at `level`.
pub fn frame_samples(profile: Profile, level: &Level, count: usize) -> usize {
    PREAMBLE + (profile.header_symbols() + level.symbols(profile, count)) * SYM
}

/// Builds the audio for one frame: preamble, header, then `chunks` at `header.level`.
/// `header.count` is set from `chunks`. `papr_db` caps the peak-to-average ratio.
pub fn build_frame(header: &Header, chunks: &[Chunk], papr_db: f32) -> Vec<f32> {
    let profile = header.profile;
    let level = LEVELS[header.level as usize];
    let mut header = header.clone();
    header.count = chunks.len() as u8;

    let synth = Synth::new(profile);
    let mut out = Vec::with_capacity(frame_samples(profile, &level, chunks.len()));
    let sync = synth.body(&profile.sync_values());
    out.extend_from_slice(&sync[NFFT - NCP..]);
    out.extend_from_slice(&sync);
    out.extend_from_slice(&sync);

    let mut symbol = 0;
    let hbits = coded(&Code::new(Length::N648, Rate::R1_2), &header.to_bytes(), 1);
    emit(
        &synth,
        Modulation::Bpsk,
        &hbits,
        &mut symbol,
        papr_db,
        &mut out,
    );

    if !chunks.is_empty() {
        let code = Code::new(level.len, level.rate);
        let mut bits = Vec::with_capacity(chunks.len() * level.n() * level.rep);
        for c in chunks {
            bits.extend(coded(&code, &c.to_bytes(level.info_bytes()), level.rep));
        }
        emit(
            &synth,
            level.modulation,
            &bits,
            &mut symbol,
            papr_db,
            &mut out,
        );
    }

    let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
    out.iter_mut().for_each(|x| *x *= TARGET_RMS / rms);
    out
}

/// Two-tone audio sent while the transmitter keys up (TXDELAY), so VOX and squelch open
/// before the preamble. The tones sit a quarter bin off-grid so they never look like a preamble.
pub fn lead_in(ms: u32) -> Vec<f32> {
    let n = FS * ms as usize / 1000;
    let ramp = (FS / 200).min(n / 2).max(1);
    let (f1, f2) = (19.25 * 62.5, 28.25 * 62.5);
    let amp = TARGET_RMS;
    (0..n)
        .map(|i| {
            let t = i as f32 / FS as f32;
            let env = (i.min(n - 1 - i) as f32 / ramp as f32).min(1.0);
            let tau = std::f32::consts::TAU;
            env * amp * ((tau * f1 * t).sin() + (tau * f2 * t).sin())
        })
        .collect()
}
