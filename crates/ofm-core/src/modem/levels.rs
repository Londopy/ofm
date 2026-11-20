use super::{Profile, FS, SYM};
use crate::ldpc::{Length, Rate};

/// Carrier modulation of a speed level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Modulation {
    Bpsk,
    Qpsk,
    Psk8,
    Qam16,
    Qam64,
}

impl Modulation {
    /// Bits carried per carrier per symbol.
    pub fn bits(self) -> usize {
        match self {
            Modulation::Bpsk => 1,
            Modulation::Qpsk => 2,
            Modulation::Psk8 => 3,
            Modulation::Qam16 => 4,
            Modulation::Qam64 => 6,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Modulation::Bpsk => "BPSK",
            Modulation::Qpsk => "QPSK",
            Modulation::Psk8 => "8PSK",
            Modulation::Qam16 => "16QAM",
            Modulation::Qam64 => "64QAM",
        }
    }
}

/// One speed level: modulation, LDPC code and repetition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub modulation: Modulation,
    pub len: Length,
    pub rate: Rate,
    /// Each coded bit is sent this many times.
    pub rep: usize,
    /// Receiver-estimated SNR above which this level stayed under 2% codeword errors in
    /// the channel simulator (see docs/curves.md).
    pub min_snr_db: i8,
}

const fn level(
    modulation: Modulation,
    len: Length,
    rate: Rate,
    rep: usize,
    min_snr_db: i8,
) -> Level {
    Level {
        modulation,
        len,
        rate,
        rep,
        min_snr_db,
    }
}

/// Speed levels, slowest first. Level 0 carries connect, ACK and ID frames.
pub const LEVELS: [Level; 10] = [
    level(Modulation::Bpsk, Length::N648, Rate::R1_2, 2, 5),
    level(Modulation::Bpsk, Length::N648, Rate::R1_2, 1, 8),
    level(Modulation::Qpsk, Length::N1944, Rate::R1_2, 1, 9),
    level(Modulation::Qpsk, Length::N1944, Rate::R3_4, 1, 11),
    level(Modulation::Psk8, Length::N1944, Rate::R2_3, 1, 12),
    level(Modulation::Qam16, Length::N1944, Rate::R2_3, 1, 13),
    level(Modulation::Qam16, Length::N1944, Rate::R3_4, 1, 15),
    level(Modulation::Qam64, Length::N1944, Rate::R2_3, 1, 18),
    level(Modulation::Qam64, Length::N1944, Rate::R3_4, 1, 19),
    level(Modulation::Qam64, Length::N1944, Rate::R5_6, 1, 21),
];

impl Level {
    /// Codeword length in bits.
    pub fn n(&self) -> usize {
        match self.len {
            Length::N648 => 648,
            Length::N1944 => 1944,
        }
    }

    /// Information bits per codeword.
    pub fn k(&self) -> usize {
        let (num, den) = match self.rate {
            Rate::R1_2 => (1, 2),
            Rate::R2_3 => (2, 3),
            Rate::R3_4 => (3, 4),
            Rate::R5_6 => (5, 6),
        };
        self.n() * num / den
    }

    /// Whole bytes that fit in one codeword.
    pub fn info_bytes(&self) -> usize {
        self.k() / 8
    }

    /// OFDM symbols needed to carry `count` codewords.
    pub fn symbols(&self, profile: Profile, count: usize) -> usize {
        let per_symbol = profile.data_carriers().len() * self.modulation.bits();
        (count * self.n() * self.rep).div_ceil(per_symbol)
    }

    /// Information rate in bits per second, before header and turnaround overhead.
    pub fn bps(&self, profile: Profile) -> f32 {
        let per_symbol = (profile.data_carriers().len() * self.modulation.bits()) as f32;
        let symbol_s = SYM as f32 / FS as f32;
        per_symbol * self.k() as f32 / self.n() as f32 / self.rep as f32 / symbol_s
    }

    /// Short description such as `16QAM r3/4`.
    pub fn describe(&self) -> String {
        let rate = match self.rate {
            Rate::R1_2 => "1/2",
            Rate::R2_3 => "2/3",
            Rate::R3_4 => "3/4",
            Rate::R5_6 => "5/6",
        };
        let rep = if self.rep > 1 {
            format!(" x{}", self.rep)
        } else {
            String::new()
        };
        format!("{} r{}{} n{}", self.modulation.name(), rate, rep, self.n())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_rates_match_spec_targets() {
        let top = LEVELS[9].bps(Profile::Narrow);
        assert!((7500.0..9000.0).contains(&top), "top narrow {top}");
        let floor = LEVELS[0].bps(Profile::Narrow);
        assert!((350.0..600.0).contains(&floor), "floor narrow {floor}");
        let wide = LEVELS[9].bps(Profile::Wide);
        assert!(wide > 1.9 * top, "wide {wide}");
    }

    #[test]
    fn levels_get_faster() {
        for p in [Profile::Narrow, Profile::Wide] {
            for w in LEVELS.windows(2) {
                assert!(w[1].bps(p) > w[0].bps(p));
            }
        }
    }
}
