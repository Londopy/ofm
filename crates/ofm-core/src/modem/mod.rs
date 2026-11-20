//! OFDM waveform for FM audio channels: carrier plan, speed levels, frame build and receive.

mod frame;
mod levels;
mod qam;
mod rx;
mod tx;

#[cfg(test)]
mod tests;

pub use frame::{Chunk, Header, Kind, HEADER_LEN};
pub use levels::{Level, Modulation, LEVELS};
pub use rx::{Receiver, RxEvent, RxFrame};
pub use tx::{build_frame, frame_samples, lead_in};

use num_complex::Complex32;

/// Audio sample rate used inside the modem.
pub const FS: usize = 48_000;
/// FFT size; 48000 / 768 gives the 62.5 Hz carrier spacing.
pub const NFFT: usize = 768;
/// Cyclic prefix, 2 ms.
pub const NCP: usize = 96;
/// One OFDM symbol including its prefix, 18 ms.
pub const SYM: usize = NFFT + NCP;
/// Preamble: a prefix and two copies of the sync symbol.
pub const PREAMBLE: usize = NCP + 2 * NFFT;
/// Most chunks in one frame; the ACK bitmap has 40 bits.
pub const MAX_CHUNKS: usize = 40;

// bit interleaver stride, coprime to both 648 and 1944
const STRIDE: usize = 101;

/// Audio path the waveform is shaped for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Mic/speaker or 1200-baud data jack, about 500 to 2700 Hz.
    Narrow,
    /// Flat 9600 discriminator port, about 300 to 4800 Hz.
    Wide,
}

impl Profile {
    /// Number of carriers, data and pilot together.
    pub fn carriers(self) -> usize {
        match self {
            Profile::Narrow => 35,
            Profile::Wide => 72,
        }
    }

    /// Name used in `CONNECTED` lines.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Narrow => "NARROW",
            Profile::Wide => "WIDE",
        }
    }

    pub(crate) fn bin(self, carrier: usize) -> usize {
        let first = match self {
            Profile::Narrow => 9,
            Profile::Wide => 5,
        };
        first + carrier
    }

    pub(crate) fn is_pilot(self, carrier: usize) -> bool {
        carrier.is_multiple_of(8) || carrier == self.carriers() - 1
    }

    pub(crate) fn data_carriers(self) -> Vec<usize> {
        (0..self.carriers())
            .filter(|&c| !self.is_pilot(c))
            .collect()
    }

    pub(crate) fn pilot_carriers(self) -> Vec<usize> {
        (0..self.carriers()).filter(|&c| self.is_pilot(c)).collect()
    }

    /// OFDM symbols taken by the header codeword.
    pub fn header_symbols(self) -> usize {
        648usize.div_ceil(self.data_carriers().len())
    }

    // zadoff-chu values for the sync symbol, one per carrier
    pub(crate) fn sync_values(self) -> Vec<Complex32> {
        let n = self.carriers();
        (0..n)
            .map(|c| {
                let c = c as f32;
                let arg = if n % 2 == 1 { c * (c + 1.0) } else { c * c };
                Complex32::from_polar(1.0, -std::f32::consts::PI * arg / n as f32)
            })
            .collect()
    }
}

// stateless hash so tx and rx agree on pilot signs and pad bits without sharing state
pub(crate) fn pn(a: u32, b: u32) -> bool {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77) ^ 0x2545_F491;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    x & 1 == 1
}

// whitens the coded bits so repetitive data (runs of zeros, say) can't line every carrier
// up in phase and blow the peak-to-average ratio; slot counts bits within the symbol
pub(crate) fn scramble(symbol: usize, slot: usize) -> u8 {
    u8::from(pn(symbol as u32 | 0x8000_0000, slot as u32))
}

pub(crate) fn pilot_value(symbol: usize, carrier: usize) -> f32 {
    if pn(symbol as u32, carrier as u32) {
        -1.0
    } else {
        1.0
    }
}

// position of coded bit i (and of its repeat) in the transmitted order
pub(crate) fn interleave(i: usize, n: usize, copy: usize) -> usize {
    (i * STRIDE + copy * n / 2) % n
}

pub(crate) fn bytes_to_bits(bytes: &[u8], k: usize) -> Vec<u8> {
    let mut bits = vec![0u8; k];
    for (i, bit) in bits.iter_mut().enumerate().take(bytes.len() * 8) {
        *bit = (bytes[i / 8] >> (7 - i % 8)) & 1;
    }
    bits
}

pub(crate) fn bits_to_bytes(bits: &[u8]) -> Vec<u8> {
    bits.as_chunks::<8>()
        .0
        .iter()
        .map(|b| b.iter().fold(0u8, |acc, &x| (acc << 1) | x))
        .collect()
}
