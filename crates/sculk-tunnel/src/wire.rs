//! The tunnel's datagrams: a 24-byte header, then a piece of the byte stream (deflated when that makes it smaller).
//!
//! ```text
//! magic u16 | kind u8 | flags u8 | session u64 | seq u32 | ack u32 | sack u32 | payload
//! ```
//!
//! Every datagram carries the receiver's acknowledgement: `ack` is the next sequence number expected (everything
//! before it arrived), bit `i` of `sack` says `ack + 1 + i` arrived too.

use std::io::{Read, Write};

const MAGIC: u16 = 0x5c17;
pub const HEADER: usize = 24;
/// The most stream bytes in one datagram: with the header it stays under a typical path MTU.
pub const MSS: usize = 1150;
/// `flags` bit: the payload is deflated.
const DEFLATED: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Client to server: open `session`.
    Hello = 1,
    /// Server to client: the session is open.
    Welcome = 2,
    /// A piece of the stream (`seq`), with the sender's acknowledgement.
    Data = 3,
    /// Only the acknowledgement.
    Ack = 4,
    /// The session ends.
    Close = 5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: Kind,
    pub session: u64,
    pub seq: u32,
    pub ack: u32,
    pub sack: u32,
    /// Stream bytes, as they go on the wire (deflated when `deflated`).
    pub payload: Vec<u8>,
    pub deflated: bool,
}

impl Segment {
    pub fn control(kind: Kind, session: u64) -> Self {
        Self { kind, session, seq: 0, ack: 0, sack: 0, payload: Vec::new(), deflated: false }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.payload.len());
        out.extend_from_slice(&MAGIC.to_be_bytes());
        out.push(self.kind as u8);
        out.push(if self.deflated { DEFLATED } else { 0 });
        out.extend_from_slice(&self.session.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.ack.to_be_bytes());
        out.extend_from_slice(&self.sack.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// `None` for anything that is not a tunnel datagram.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || u16::from_be_bytes([bytes[0], bytes[1]]) != MAGIC {
            return None;
        }
        let kind = match bytes[2] {
            1 => Kind::Hello,
            2 => Kind::Welcome,
            3 => Kind::Data,
            4 => Kind::Ack,
            5 => Kind::Close,
            _ => return None,
        };
        let u32_at = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
        Some(Self {
            kind,
            deflated: bytes[3] & DEFLATED != 0,
            session: u64::from_be_bytes(bytes[4..12].try_into().expect("8 bytes")),
            seq: u32_at(12),
            ack: u32_at(16),
            sack: u32_at(20),
            payload: bytes[HEADER..].to_vec(),
        })
    }
}

/// The payload for `data`: deflated when that is smaller (Minecraft compresses only packets of 256 bytes or more,
/// so the many small ones gain), and whether it was.
pub fn pack(data: &[u8]) -> (Vec<u8>, bool) {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    if encoder.write_all(data).is_ok()
        && let Ok(deflated) = encoder.finish()
        && deflated.len() < data.len()
    {
        return (deflated, true);
    }
    (data.to_vec(), false)
}

/// The stream bytes a payload carries.
pub fn unpack(payload: &[u8], deflated: bool) -> Option<Vec<u8>> {
    if !deflated {
        return Some(payload.to_vec());
    }
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(payload).take(MSS as u64 * 64).read_to_end(&mut out).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_round_trip() {
        let (payload, deflated) = pack(&[7; 500]);
        assert!(deflated && payload.len() < 50);
        let s = Segment { kind: Kind::Data, session: 9, seq: 3, ack: 2, sack: 0b101, payload, deflated };
        let back = Segment::decode(&s.encode()).unwrap();
        assert_eq!(back, s);
        assert_eq!(unpack(&back.payload, back.deflated).unwrap(), vec![7; 500]);
        assert_eq!(Segment::decode(b"\x16\x03\x01 a TLS hello, not ours"), None);
    }

    #[test]
    fn incompressible_bytes_go_as_they_are() {
        let data: Vec<u8> = (0..200u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let (payload, deflated) = pack(&data);
        assert!(!deflated);
        assert_eq!(payload, data);
    }
}
