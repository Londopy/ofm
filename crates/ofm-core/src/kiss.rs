//! KISS framing for the connectionless port, and packing a KISS frame into one radio frame.

use crate::modem::{Chunk, Header, Kind, Profile, RxFrame, LEVELS, MAX_CHUNKS};

const FEND: u8 = 0xC0;
const FESC: u8 = 0xDB;
const TFEND: u8 = 0xDC;
const TFESC: u8 = 0xDD;

/// Wraps `data` as a KISS data frame on port 0.
pub fn encode(data: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, 0x00];
    for &b in data {
        match b {
            FEND => out.extend([FESC, TFEND]),
            FESC => out.extend([FESC, TFESC]),
            _ => out.push(b),
        }
    }
    out.push(FEND);
    out
}

/// Pulls KISS data frames out of a TCP byte stream. Non-data commands
/// (TXDELAY, persistence and so on) are dropped; ofm takes those from its config.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    escape: bool,
    // the frame outgrew MAX_FRAME and is dropped at its FEND
    overflow: bool,
}

// well over the biggest payload one radio frame can carry
const MAX_FRAME: usize = 16 * 1024;

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for &b in bytes {
            match (b, self.escape) {
                (FEND, _) => {
                    if !self.overflow && self.buf.len() > 1 && self.buf[0] & 0x0f == 0 {
                        frames.push(self.buf[1..].to_vec());
                    }
                    self.buf.clear();
                    self.escape = false;
                    self.overflow = false;
                }
                _ if self.buf.len() >= MAX_FRAME => self.overflow = true,
                (FESC, false) => self.escape = true,
                (TFEND, true) => {
                    self.buf.push(FEND);
                    self.escape = false;
                }
                (TFESC, true) => {
                    self.buf.push(FESC);
                    self.escape = false;
                }
                (b, _) => {
                    self.buf.push(b);
                    self.escape = false;
                }
            }
        }
        frames
    }
}

/// Builds one UI radio frame carrying `payload` from `src` at `level`.
/// Returns None if the payload does not fit in one frame at that level.
pub fn ui_frame(
    src: &str,
    payload: &[u8],
    profile: Profile,
    level: u8,
) -> Option<(Header, Vec<Chunk>)> {
    let cap = Chunk::capacity(LEVELS.get(level as usize)?.info_bytes());
    let chunks: Vec<Chunk> = payload
        .chunks(cap)
        .enumerate()
        .map(|(i, d)| Chunk {
            offset: (i * cap) as u32,
            data: d.to_vec(),
        })
        .collect();
    if chunks.is_empty() || chunks.len() > MAX_CHUNKS {
        return None;
    }
    let mut h = Header::new(Kind::Ui, profile, src, "QST");
    h.level = level;
    Some((h, chunks))
}

/// The payload of a received UI frame, only if every chunk arrived intact.
pub fn ui_payload(f: &RxFrame) -> Option<Vec<u8>> {
    if f.header.kind != Kind::Ui {
        return None;
    }
    let mut out = Vec::new();
    for c in &f.chunks {
        let c = c.as_ref()?;
        if c.offset as usize != out.len() {
            return None;
        }
        out.extend_from_slice(&c.data);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiss_roundtrip_with_escapes() {
        let data = vec![1, FEND, 2, FESC, 3];
        let wire = encode(&data);
        assert_eq!(wire, vec![FEND, 0, 1, FESC, TFEND, 2, FESC, TFESC, 3, FEND]);
        let mut d = Decoder::default();
        assert_eq!(d.feed(&wire[..4]), Vec::<Vec<u8>>::new());
        assert_eq!(d.feed(&wire[4..]), vec![data]);
    }

    #[test]
    fn kiss_commands_are_not_data() {
        let mut d = Decoder::default();
        assert!(d.feed(&[FEND, 0x01, 50, FEND]).is_empty());
    }

    #[test]
    fn ui_frame_needs_every_chunk() {
        let payload: Vec<u8> = (0..200u8).collect();
        let (h, chunks) = ui_frame("N0CALL", &payload, Profile::Narrow, 1).unwrap();
        assert!(chunks.len() > 1);
        let mut f = RxFrame {
            header: h,
            chunks: chunks.into_iter().map(Some).collect(),
            snr_db: 20.0,
        };
        assert_eq!(ui_payload(&f), Some(payload));
        f.chunks[1] = None;
        assert_eq!(ui_payload(&f), None);
    }
}
