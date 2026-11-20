use super::Profile;

/// Header size in bytes; it rides in one 648-bit rate 1/2 codeword.
pub const HEADER_LEN: usize = 40;
const BODY_LEN: usize = 12;
const VERSION: u8 = 1;

/// What a frame is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Data,
    Connect,
    Accept,
    Reject,
    Ack,
    Disconnect,
    DisconnectAck,
    Id,
    /// Connectionless frame from the KISS port.
    Ui,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::Data => 0,
            Kind::Connect => 1,
            Kind::Accept => 2,
            Kind::Reject => 3,
            Kind::Ack => 4,
            Kind::Disconnect => 5,
            Kind::DisconnectAck => 6,
            Kind::Id => 7,
            Kind::Ui => 8,
        }
    }

    fn from_code(c: u8) -> Option<Kind> {
        Some(match c {
            0 => Kind::Data,
            1 => Kind::Connect,
            2 => Kind::Accept,
            3 => Kind::Reject,
            4 => Kind::Ack,
            5 => Kind::Disconnect,
            6 => Kind::DisconnectAck,
            7 => Kind::Id,
            8 => Kind::Ui,
            _ => return None,
        })
    }
}

/// Frame header, always sent at the most robust setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub profile: Profile,
    /// Speed level of the chunks that follow.
    pub level: u8,
    /// Number of chunks (codewords) after the header.
    pub count: u8,
    pub seq: u8,
    /// Sending station, in plain ASCII so the frame identifies it.
    pub src: String,
    pub dst: String,
    /// Kind-specific fields, such as the ACK bitmap.
    pub body: [u8; BODY_LEN],
}

fn put_call(out: &mut [u8], call: &str) {
    out.fill(b' ');
    for (o, c) in out.iter_mut().zip(call.bytes()) {
        *o = c.to_ascii_uppercase();
    }
}

fn get_call(b: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(b).ok()?.trim_end();
    s.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'/')
        .then(|| s.to_string())
}

impl Header {
    pub fn new(kind: Kind, profile: Profile, src: &str, dst: &str) -> Header {
        Header {
            kind,
            profile,
            level: 0,
            count: 0,
            seq: 0,
            src: src.to_ascii_uppercase(),
            dst: dst.to_ascii_uppercase(),
            body: [0; BODY_LEN],
        }
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = (VERSION << 4) | self.kind.code();
        b[1] = (u8::from(self.profile == Profile::Wide) << 7) | (self.level & 0x7f);
        b[2] = self.count;
        b[3] = self.seq;
        put_call(&mut b[4..14], &self.src);
        put_call(&mut b[14..24], &self.dst);
        b[24..36].copy_from_slice(&self.body);
        let crc = crc32fast::hash(&b[..36]);
        b[36..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Parses and checks a header, returning None on a bad CRC or unknown version.
    pub fn from_bytes(b: &[u8]) -> Option<Header> {
        if b.len() < HEADER_LEN || crc32fast::hash(&b[..36]).to_le_bytes() != b[36..40] {
            return None;
        }
        if b[0] >> 4 != VERSION {
            return None;
        }
        let profile = if b[1] & 0x80 != 0 {
            Profile::Wide
        } else {
            Profile::Narrow
        };
        Some(Header {
            kind: Kind::from_code(b[0] & 0x0f)?,
            profile,
            level: b[1] & 0x7f,
            count: b[2],
            seq: b[3],
            src: get_call(&b[4..14])?,
            dst: get_call(&b[14..24])?,
            body: b[24..36].try_into().ok()?,
        })
    }
}

/// The payload of one codeword: a byte offset into the stream and up to `capacity` bytes,
/// CRC protected. Offsets rather than counters let a chunk be re-cut at any level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u32,
    pub data: Vec<u8>,
}

impl Chunk {
    /// Payload bytes that fit in a codeword of `info_bytes`.
    pub fn capacity(info_bytes: usize) -> usize {
        info_bytes - 9
    }

    pub fn to_bytes(&self, info_bytes: usize) -> Vec<u8> {
        assert!(self.data.len() <= Self::capacity(info_bytes));
        let mut b = Vec::with_capacity(info_bytes);
        b.extend_from_slice(&self.offset.to_le_bytes());
        b.push(self.data.len() as u8);
        b.extend_from_slice(&self.data);
        let crc = crc32fast::hash(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b.resize(info_bytes, 0);
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Chunk> {
        let len = *b.get(4)? as usize;
        let end = 5 + len;
        let crc = b.get(end..end + 4)?;
        (crc32fast::hash(&b[..end]).to_le_bytes() == crc).then(|| Chunk {
            offset: u32::from_le_bytes(b[..4].try_into().unwrap()),
            data: b[5..end].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_crc() {
        let mut h = Header::new(Kind::Connect, Profile::Wide, "n0call-7", "w1aw");
        h.level = 5;
        h.count = 3;
        h.seq = 200;
        h.body[0] = 0xab;
        let mut b = h.to_bytes();
        let back = Header::from_bytes(&b).unwrap();
        assert_eq!(back, h);
        assert_eq!(back.src, "N0CALL-7");
        b[5] ^= 1;
        assert!(Header::from_bytes(&b).is_none());
    }

    #[test]
    fn callsigns_are_plain_ascii_in_the_frame() {
        let b = Header::new(Kind::Id, Profile::Narrow, "N0CALL", "ID").to_bytes();
        assert_eq!(&b[4..14], b"N0CALL    ");
    }

    #[test]
    fn chunk_roundtrip_and_crc() {
        let c = Chunk {
            offset: 70_000,
            data: b"hello winlink".to_vec(),
        };
        let mut b = c.to_bytes(121);
        assert_eq!(b.len(), 121);
        assert_eq!(Chunk::from_bytes(&b), Some(c));
        b[4] ^= 0x10;
        assert_eq!(Chunk::from_bytes(&b), None);
    }
}
