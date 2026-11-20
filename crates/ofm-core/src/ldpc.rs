/// Code rate, info bits over codeword bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rate {
    R1_2,
    R2_3,
    R3_4,
    R5_6,
}

/// Codeword length in bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Length {
    N648,
    N1944,
}

/// An IEEE 802.11n QC-LDPC code: a 24-column base matrix whose entries expand to z x z blocks.
#[derive(Clone, Debug)]
pub struct Code {
    z: usize,
    base: &'static [[i8; 24]],
}

impl Code {
    /// Picks the base matrix for this length and rate; cheap enough to call per frame.
    pub fn new(len: Length, rate: Rate) -> Code {
        let z = match len {
            Length::N648 => 27,
            Length::N1944 => 81,
        };
        let base: &'static [[i8; 24]] = match (len, rate) {
            (Length::N648, Rate::R1_2) => &N648_R1_2,
            (Length::N648, Rate::R2_3) => &N648_R2_3,
            (Length::N648, Rate::R3_4) => &N648_R3_4,
            (Length::N648, Rate::R5_6) => &N648_R5_6,
            (Length::N1944, Rate::R1_2) => &N1944_R1_2,
            (Length::N1944, Rate::R2_3) => &N1944_R2_3,
            (Length::N1944, Rate::R3_4) => &N1944_R3_4,
            (Length::N1944, Rate::R5_6) => &N1944_R5_6,
        };
        Code { z, base }
    }

    /// Codeword bits.
    pub fn n(&self) -> usize {
        24 * self.z
    }

    /// Info bits.
    pub fn k(&self) -> usize {
        (24 - self.base.len()) * self.z
    }

    /// info: k bits, each 0 or 1. Returns n bits, systematic (info bits first, then parity).
    pub fn encode(&self, info: &[u8]) -> Vec<u8> {
        assert_eq!(info.len(), self.k(), "info must hold k bits");
        let (z, k) = (self.z, self.k());
        let mut cw = info.to_vec();
        cw.resize(self.n(), 0);
        // with the parity still zero each row sum is that row's lambda, and p0 is their sum
        let mut p0 = vec![0; z];
        for row in self.base {
            for (p, l) in p0.iter_mut().zip(self.row_sum(row, &cw)) {
                *p ^= l;
            }
        }
        cw[k..k + z].copy_from_slice(&p0);
        // down the staircase, row i - 1 has parity block i as its only unknown left
        for i in 1..self.base.len() {
            let p = self.row_sum(&self.base[i - 1], &cw);
            cw[k + i * z..][..z].copy_from_slice(&p);
        }
        cw
    }

    /// llr: n values, positive means bit 0 is more likely. Returns the k info bits only if
    /// all parity checks are satisfied within max_iter iterations, else None.
    pub fn decode(&self, llr: &[f32], max_iter: usize) -> Option<Vec<u8>> {
        assert_eq!(llr.len(), self.n(), "llr must hold n values");
        let mut post = llr.to_vec();
        let mut c2v: Vec<Vec<f32>> = self
            .base
            .iter()
            .map(|row| vec![0.0; blocks(row).count() * self.z])
            .collect();
        let mut v2c = vec![0.0; self.n()];
        let mut layer = Layer::new(self.z);
        for _ in 0..max_iter {
            if let Some(info) = self.decide(&post) {
                return Some(info);
            }
            for (row, msgs) in self.base.iter().zip(&mut c2v) {
                self.update_layer(row, msgs, &mut post, &mut v2c, &mut layer);
            }
        }
        self.decide(&post)
    }

    // normalized min-sum over the z checks of one base row, updating post in place
    fn update_layer(
        &self,
        row: &[i8; 24],
        msgs: &mut [f32],
        post: &mut [f32],
        v2c: &mut [f32],
        layer: &mut Layer,
    ) {
        let z = self.z;
        layer.clear();
        for ((col, s), old) in blocks(row).zip(msgs.chunks_exact(z)) {
            // in check order: entry r is variable (r + s) % z of the block
            let q = &mut v2c[col * z..][..z];
            q.copy_from_slice(&post[col * z..][..z]);
            q.rotate_left(s);
            for (x, m) in q.iter_mut().zip(old) {
                *x -= m;
            }
            layer.add(q);
        }
        for ((col, s), new) in blocks(row).zip(msgs.chunks_exact_mut(z)) {
            let q = &mut v2c[col * z..][..z];
            layer.reply(q, new);
            for (x, m) in q.iter_mut().zip(new.iter()) {
                *x += m;
            }
            q.rotate_right(s);
            post[col * z..][..z].copy_from_slice(q);
        }
    }

    // info bits of the hard decisions, if those satisfy every check
    fn decide(&self, post: &[f32]) -> Option<Vec<u8>> {
        let mut bits: Vec<u8> = post.iter().map(|&x| u8::from(x < 0.0)).collect();
        let ok = self
            .base
            .iter()
            .all(|row| self.row_sum(row, &bits).iter().all(|&b| b == 0));
        ok.then(|| {
            bits.truncate(self.k());
            bits
        })
    }

    // xor of the row's blocks, each multiplied by its shifted identity; zero where the checks hold
    fn row_sum(&self, row: &[i8; 24], bits: &[u8]) -> Vec<u8> {
        let z = self.z;
        let mut sum = vec![0; z];
        for (col, s) in blocks(row) {
            let b = &bits[col * z..][..z];
            for (x, y) in sum.iter_mut().zip(b[s..].iter().chain(&b[..s])) {
                *x ^= y;
            }
        }
        sum
    }
}

// nonzero blocks of a base row as (column, shift)
fn blocks(row: &[i8; 24]) -> impl Iterator<Item = (usize, usize)> + '_ {
    row.iter()
        .enumerate()
        .filter(|&(_, &s)| s >= 0)
        .map(|(col, &s)| (col, s as usize))
}

// min-sum state of a layer's z checks, kept as plain arrays so the branch-free loops vectorize
struct Layer {
    min1: Vec<f32>,
    min2: Vec<f32>,
    neg: Vec<bool>,
}

impl Layer {
    fn new(z: usize) -> Layer {
        Layer {
            min1: vec![0.0; z],
            min2: vec![0.0; z],
            neg: vec![false; z],
        }
    }

    fn clear(&mut self) {
        self.min1.fill(f32::INFINITY);
        self.min2.fill(f32::INFINITY);
        self.neg.fill(false);
    }

    // fold in one block of variable-to-check messages, in check order
    fn add(&mut self, q: &[f32]) {
        let state = self.min1.iter_mut().zip(&mut self.min2).zip(&mut self.neg);
        for (&x, ((m1, m2), neg)) in q.iter().zip(state) {
            let a = x.abs();
            *m2 = m2.min(a.max(*m1));
            *m1 = m1.min(a);
            *neg ^= x < 0.0;
        }
    }

    // replies to the block that sent q, each built from every input but its own
    fn reply(&self, q: &[f32], out: &mut [f32]) {
        let state = self.min1.iter().zip(&self.min2).zip(&self.neg);
        for ((&x, m), ((&m1, &m2), &neg)) in q.iter().zip(out).zip(state) {
            // the input that set min1 gets min2; on a tie the two are equal anyway
            let min = if x.abs() == m1 { m2 } else { m1 };
            // scaled down because min-sum overestimates the true check message
            let mag = 0.75 * min;
            *m = if neg == (x < 0.0) { mag } else { -mag };
        }
    }
}

// 802.11n base matrices: the shift of each z x z block, -1 for a zero block
#[rustfmt::skip]
const N648_R1_2: [[i8; 24]; 12] = [
    [0, -1, -1, -1, 0, 0, -1, -1, 0, -1, -1, 0, 1, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [22, 0, -1, -1, 17, -1, 0, 0, 12, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [6, -1, 0, -1, 10, -1, -1, -1, 24, -1, 0, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1, -1],
    [2, -1, -1, 0, 20, -1, -1, -1, 25, 0, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1],
    [23, -1, -1, -1, 3, -1, -1, -1, 0, -1, 9, 11, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1],
    [24, -1, 23, 1, 17, -1, 3, -1, 10, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1],
    [25, -1, -1, -1, 8, -1, -1, -1, 7, 18, -1, -1, 0, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1],
    [13, 24, -1, -1, 0, -1, 8, -1, 6, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1],
    [7, 20, -1, 16, 22, 10, -1, -1, 23, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1],
    [11, -1, -1, -1, 19, -1, -1, -1, 13, -1, 3, 17, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1],
    [25, -1, 8, -1, 23, 18, -1, 14, 9, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0],
    [3, -1, -1, -1, 16, -1, -1, 2, 25, 5, -1, -1, 1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N648_R2_3: [[i8; 24]; 8] = [
    [25, 26, 14, -1, 20, -1, 2, -1, 4, -1, -1, 8, -1, 16, -1, 18, 1, 0, -1, -1, -1, -1, -1, -1],
    [10, 9, 15, 11, -1, 0, -1, 1, -1, -1, 18, -1, 8, -1, 10, -1, -1, 0, 0, -1, -1, -1, -1, -1],
    [16, 2, 20, 26, 21, -1, 6, -1, 1, 26, -1, 7, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1],
    [10, 13, 5, 0, -1, 3, -1, 7, -1, -1, 26, -1, -1, 13, -1, 16, -1, -1, -1, 0, 0, -1, -1, -1],
    [23, 14, 24, -1, 12, -1, 19, -1, 17, -1, -1, -1, 20, -1, 21, -1, 0, -1, -1, -1, 0, 0, -1, -1],
    [6, 22, 9, 20, -1, 25, -1, 17, -1, 8, -1, 14, -1, 18, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1],
    [14, 23, 21, 11, 20, -1, 24, -1, 18, -1, 19, -1, -1, -1, -1, 22, -1, -1, -1, -1, -1, -1, 0, 0],
    [17, 11, 11, 20, -1, 21, -1, 26, -1, 3, -1, -1, 18, -1, 26, -1, 1, -1, -1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N648_R3_4: [[i8; 24]; 6] = [
    [16, 17, 22, 24, 9, 3, 14, -1, 4, 2, 7, -1, 26, -1, 2, -1, 21, -1, 1, 0, -1, -1, -1, -1],
    [25, 12, 12, 3, 3, 26, 6, 21, -1, 15, 22, -1, 15, -1, 4, -1, -1, 16, -1, 0, 0, -1, -1, -1],
    [25, 18, 26, 16, 22, 23, 9, -1, 0, -1, 4, -1, 4, -1, 8, 23, 11, -1, -1, -1, 0, 0, -1, -1],
    [9, 7, 0, 1, 17, -1, -1, 7, 3, -1, 3, 23, -1, 16, -1, -1, 21, -1, 0, -1, -1, 0, 0, -1],
    [24, 5, 26, 7, 1, -1, -1, 15, 24, 15, -1, 8, -1, 13, -1, 13, -1, 11, -1, -1, -1, -1, 0, 0],
    [2, 2, 19, 14, 24, 1, 15, 19, -1, 21, -1, 2, -1, 24, -1, 3, -1, 2, 1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N648_R5_6: [[i8; 24]; 4] = [
    [17, 13, 8, 21, 9, 3, 18, 12, 10, 0, 4, 15, 19, 2, 5, 10, 26, 19, 13, 13, 1, 0, -1, -1],
    [3, 12, 11, 14, 11, 25, 5, 18, 0, 9, 2, 26, 26, 10, 24, 7, 14, 20, 4, 2, -1, 0, 0, -1],
    [22, 16, 4, 3, 10, 21, 12, 5, 21, 14, 19, 5, -1, 8, 5, 18, 11, 5, 5, 15, 0, -1, 0, 0],
    [7, 7, 14, 14, 4, 16, 16, 24, 24, 10, 1, 7, 15, 6, 10, 26, 8, 18, 21, 14, 1, -1, -1, 0],
];

#[rustfmt::skip]
const N1944_R1_2: [[i8; 24]; 12] = [
    [57, -1, -1, -1, 50, -1, 11, -1, 50, -1, 79, -1, 1, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [3, -1, 28, -1, 0, -1, -1, -1, 55, 7, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1, -1, -1],
    [30, -1, -1, -1, 24, 37, -1, -1, 56, 14, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1, -1],
    [62, 53, -1, -1, 53, -1, -1, 3, 35, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1, -1],
    [40, -1, -1, 20, 66, -1, -1, 22, 28, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1, -1],
    [0, -1, -1, -1, 8, -1, 42, -1, 50, -1, -1, 8, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1],
    [69, 79, 79, -1, -1, -1, 56, -1, 52, -1, -1, -1, 0, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1],
    [65, -1, -1, -1, 38, 57, -1, -1, 72, -1, 27, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1],
    [64, -1, -1, -1, 14, 52, -1, -1, 30, -1, -1, 32, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1, -1],
    [-1, 45, -1, 70, 0, -1, -1, -1, 77, 9, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, -1],
    [2, 56, -1, 57, 35, -1, -1, -1, -1, -1, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0],
    [24, -1, 61, -1, 60, -1, -1, 27, 51, -1, -1, 16, 1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N1944_R2_3: [[i8; 24]; 8] = [
    [61, 75, 4, 63, 56, -1, -1, -1, -1, -1, -1, 8, -1, 2, 17, 25, 1, 0, -1, -1, -1, -1, -1, -1],
    [56, 74, 77, 20, -1, -1, -1, 64, 24, 4, 67, -1, 7, -1, -1, -1, -1, 0, 0, -1, -1, -1, -1, -1],
    [28, 21, 68, 10, 7, 14, 65, -1, -1, -1, 23, -1, -1, -1, 75, -1, -1, -1, 0, 0, -1, -1, -1, -1],
    [48, 38, 43, 78, 76, -1, -1, -1, -1, 5, 36, -1, 15, 72, -1, -1, -1, -1, -1, 0, 0, -1, -1, -1],
    [40, 2, 53, 25, -1, 52, 62, -1, 20, -1, -1, 44, -1, -1, -1, -1, 0, -1, -1, -1, 0, 0, -1, -1],
    [69, 23, 64, 10, 22, -1, 21, -1, -1, -1, -1, -1, 68, 23, 29, -1, -1, -1, -1, -1, -1, 0, 0, -1],
    [12, 0, 68, 20, 55, 61, -1, 40, -1, -1, -1, 52, -1, -1, -1, 44, -1, -1, -1, -1, -1, -1, 0, 0],
    [58, 8, 34, 64, 78, -1, -1, 11, 78, 24, -1, -1, -1, -1, -1, 58, 1, -1, -1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N1944_R3_4: [[i8; 24]; 6] = [
    [48, 29, 28, 39, 9, 61, -1, -1, -1, 63, 45, 80, -1, -1, -1, 37, 32, 22, 1, 0, -1, -1, -1, -1],
    [4, 49, 42, 48, 11, 30, -1, -1, -1, 49, 17, 41, 37, 15, -1, 54, -1, -1, -1, 0, 0, -1, -1, -1],
    [35, 76, 78, 51, 37, 35, 21, -1, 17, 64, -1, -1, -1, 59, 7, -1, -1, 32, -1, -1, 0, 0, -1, -1],
    [9, 65, 44, 9, 54, 56, 73, 34, 42, -1, -1, -1, 35, -1, -1, -1, 46, 39, 0, -1, -1, 0, 0, -1],
    [3, 62, 7, 80, 68, 26, -1, 80, 55, -1, 36, -1, 26, -1, 9, -1, 72, -1, -1, -1, -1, -1, 0, 0],
    [26, 75, 33, 21, 69, 59, 3, 38, -1, -1, -1, 35, -1, 62, 36, 26, -1, -1, 1, -1, -1, -1, -1, 0],
];

#[rustfmt::skip]
const N1944_R5_6: [[i8; 24]; 4] = [
    [13, 48, 80, 66, 4, 74, 7, 30, 76, 52, 37, 60, -1, 49, 73, 31, 74, 73, 23, -1, 1, 0, -1, -1],
    [69, 63, 74, 56, 64, 77, 57, 65, 6, 16, 51, -1, 64, -1, 68, 9, 48, 62, 54, 27, -1, 0, 0, -1],
    [51, 15, 0, 80, 24, 25, 42, 54, 44, 71, 71, 9, 67, 35, -1, 58, -1, 29, -1, 53, 0, -1, 0, 0],
    [16, 29, 36, 41, 44, 56, 59, 37, 50, 24, -1, 65, 4, 65, 52, -1, 4, -1, 73, 52, 1, -1, -1, 0],
];

#[cfg(test)]
mod tests {
    use super::*;

    const CODES: [(Length, Rate, usize, usize); 8] = [
        (Length::N648, Rate::R1_2, 648, 324),
        (Length::N648, Rate::R2_3, 648, 432),
        (Length::N648, Rate::R3_4, 648, 486),
        (Length::N648, Rate::R5_6, 648, 540),
        (Length::N1944, Rate::R1_2, 1944, 972),
        (Length::N1944, Rate::R2_3, 1944, 1296),
        (Length::N1944, Rate::R3_4, 1944, 1458),
        (Length::N1944, Rate::R5_6, 1944, 1620),
    ];

    // xorshift64
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn bits(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| (self.next_u64() >> 63) as u8).collect()
        }

        // standard normal, by Box-Muller
        fn gauss(&mut self) -> f32 {
            let scale = 1.0 / (1u64 << 53) as f64;
            let u = ((self.next_u64() >> 11) as f64 + 0.5) * scale;
            let v = (self.next_u64() >> 11) as f64 * scale;
            ((-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()) as f32
        }
    }

    // the variables of every check, expanded straight from the base matrix
    fn parity_checks(code: &Code) -> Vec<Vec<usize>> {
        let z = code.z;
        let mut checks = Vec::new();
        for row in code.base {
            for r in 0..z {
                let vars = (0..24)
                    .filter(|&j| row[j] >= 0)
                    .map(|j| j * z + (r + row[j] as usize) % z)
                    .collect();
                checks.push(vars);
            }
        }
        checks
    }

    // channel llrs for bpsk over awgn at the given eb/n0
    fn awgn(code: &Code, cw: &[u8], ebn0_db: f32, rng: &mut Rng) -> Vec<f32> {
        let rate = code.k() as f32 / code.n() as f32;
        let var = 1.0 / (2.0 * rate * 10f32.powf(ebn0_db / 10.0));
        cw.iter()
            .map(|&b| {
                let y = 1.0 - 2.0 * f32::from(b) + var.sqrt() * rng.gauss();
                2.0 * y / var
            })
            .collect()
    }

    #[test]
    fn sizes() {
        for (len, rate, n, k) in CODES {
            let code = Code::new(len, rate);
            assert_eq!((code.n(), code.k()), (n, k), "{len:?} {rate:?}");
        }
    }

    #[test]
    fn encode_satisfies_every_check() {
        let mut rng = Rng(1);
        for (len, rate, n, k) in CODES {
            let code = Code::new(len, rate);
            let checks = parity_checks(&code);
            assert_eq!(checks.len(), n - k);
            for _ in 0..5 {
                let info = rng.bits(k);
                let cw = code.encode(&info);
                assert_eq!(cw.len(), n);
                assert_eq!(&cw[..k], &info[..], "{len:?} {rate:?} is not systematic");
                for vars in &checks {
                    let parity = vars.iter().fold(0, |p, &v| p ^ cw[v]);
                    assert_eq!(parity, 0, "{len:?} {rate:?} fails check {vars:?}");
                }
            }
        }
    }

    #[test]
    fn decodes_noiseless_llrs() {
        let mut rng = Rng(2);
        for (len, rate, _, k) in CODES {
            let code = Code::new(len, rate);
            let info = rng.bits(k);
            let llr: Vec<f32> = code
                .encode(&info)
                .iter()
                .map(|&b| if b == 0 { 4.0 } else { -4.0 })
                .collect();
            assert_eq!(code.decode(&llr, 50), Some(info), "{len:?} {rate:?}");
        }
    }

    #[test]
    fn decodes_awgn_above_threshold() {
        let mut rng = Rng(3);
        for (len, rate, _, k) in CODES {
            let code = Code::new(len, rate);
            let ebn0_db = match rate {
                Rate::R1_2 => 3.0,
                Rate::R2_3 => 4.0,
                Rate::R3_4 => 5.0,
                Rate::R5_6 => 6.0,
            };
            let mut ok = 0;
            for _ in 0..20 {
                let info = rng.bits(k);
                let llr = awgn(&code, &code.encode(&info), ebn0_db, &mut rng);
                if code.decode(&llr, 50) == Some(info) {
                    ok += 1;
                }
            }
            assert!(ok >= 19, "{len:?} {rate:?}: {ok} of 20 decoded");
        }
    }

    #[test]
    fn rejects_pure_noise() {
        let mut rng = Rng(4);
        for (len, rate, n, _) in CODES {
            let code = Code::new(len, rate);
            for _ in 0..20 {
                let llr: Vec<f32> = (0..n).map(|_| 2.0 * rng.gauss()).collect();
                assert_eq!(code.decode(&llr, 50), None, "{len:?} {rate:?}");
            }
        }
    }

    #[test]
    fn code_is_send_and_sync() {
        fn shared<T: Send + Sync>() {}
        shared::<Code>();
    }
}
