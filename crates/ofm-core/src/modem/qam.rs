use super::Modulation;
use num_complex::Complex32;
use std::sync::OnceLock;

/// Gray-mapped points, indexed by their bit label (first bit is the MSB).
pub(crate) struct Constellation {
    points: Vec<Complex32>,
    bits: usize,
}

fn gray(x: usize) -> usize {
    x ^ (x >> 1)
}

// amplitude of each label on one axis of a square qam, unit average power over both axes
fn pam(levels: usize) -> Vec<f32> {
    let norm = (2.0 * (levels * levels - 1) as f32 / 3.0).sqrt();
    let mut amp = vec![0.0; levels];
    for l in 0..levels {
        amp[gray(l)] = (2 * l) as f32 - (levels - 1) as f32;
    }
    amp.iter().map(|a| a / norm).collect()
}

fn build(m: Modulation) -> Constellation {
    let bits = m.bits();
    let points = match m {
        Modulation::Bpsk => vec![Complex32::new(1.0, 0.0), Complex32::new(-1.0, 0.0)],
        Modulation::Psk8 => {
            let mut p = vec![Complex32::default(); 8];
            for k in 0..8 {
                let phase = std::f32::consts::PI * (2 * k + 1) as f32 / 8.0;
                p[gray(k)] = Complex32::from_polar(1.0, phase);
            }
            p
        }
        Modulation::Qpsk | Modulation::Qam16 | Modulation::Qam64 => {
            let half = bits / 2;
            let axis = pam(1 << half);
            (0..1usize << bits)
                .map(|label| Complex32::new(axis[label >> half], axis[label & ((1 << half) - 1)]))
                .collect()
        }
    };
    Constellation { points, bits }
}

pub(crate) fn constellation(m: Modulation) -> &'static Constellation {
    static TABLES: OnceLock<Vec<Constellation>> = OnceLock::new();
    let all = TABLES.get_or_init(|| {
        [
            Modulation::Bpsk,
            Modulation::Qpsk,
            Modulation::Psk8,
            Modulation::Qam16,
            Modulation::Qam64,
        ]
        .into_iter()
        .map(build)
        .collect()
    });
    let i = match m {
        Modulation::Bpsk => 0,
        Modulation::Qpsk => 1,
        Modulation::Psk8 => 2,
        Modulation::Qam16 => 3,
        Modulation::Qam64 => 4,
    };
    &all[i]
}

impl Constellation {
    pub fn map(&self, bits: &[u8]) -> Complex32 {
        let label = bits.iter().fold(0usize, |acc, &b| (acc << 1) | b as usize);
        self.points[label]
    }

    /// Max-log LLRs for one received point, positive meaning bit 0.
    pub fn demap(&self, y: Complex32, noise_var: f32, out: &mut Vec<f32>) {
        let mut d0 = [f32::INFINITY; 6];
        let mut d1 = [f32::INFINITY; 6];
        for (label, p) in self.points.iter().enumerate() {
            let d = (y - p).norm_sqr();
            for b in 0..self.bits {
                let slot = if (label >> (self.bits - 1 - b)) & 1 == 0 {
                    &mut d0
                } else {
                    &mut d1
                };
                if d < slot[b] {
                    slot[b] = d;
                }
            }
        }
        for b in 0..self.bits {
            out.push((d1[b] - d0[b]) / noise_var);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Modulation; 5] = [
        Modulation::Bpsk,
        Modulation::Qpsk,
        Modulation::Psk8,
        Modulation::Qam16,
        Modulation::Qam64,
    ];

    #[test]
    fn unit_average_power() {
        for m in ALL {
            let c = constellation(m);
            let p: f32 = c.points.iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points.len() as f32;
            assert!((p - 1.0).abs() < 1e-4, "{m:?} power {p}");
        }
    }

    #[test]
    fn nearest_neighbours_differ_by_one_bit() {
        for m in ALL {
            let c = constellation(m);
            for (a, pa) in c.points.iter().enumerate() {
                let nearest = c
                    .points
                    .iter()
                    .enumerate()
                    .filter(|&(b, _)| b != a)
                    .map(|(b, pb)| ((pa - pb).norm_sqr(), b))
                    .fold((f32::INFINITY, 0), |x, y| if y.0 < x.0 { y } else { x });
                assert_eq!((a ^ nearest.1).count_ones(), 1, "{m:?} label {a}");
            }
        }
    }

    #[test]
    fn demap_recovers_bits_without_noise() {
        for m in ALL {
            let c = constellation(m);
            for label in 0..1usize << c.bits {
                let bits: Vec<u8> = (0..c.bits)
                    .map(|b| ((label >> (c.bits - 1 - b)) & 1) as u8)
                    .collect();
                let mut llr = Vec::new();
                c.demap(c.map(&bits), 0.1, &mut llr);
                let back: Vec<u8> = llr.iter().map(|&l| u8::from(l < 0.0)).collect();
                assert_eq!(back, bits, "{m:?}");
            }
        }
    }
}
