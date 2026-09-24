//! DDC/CI frame construction and reply decoding.

/// 7-bit I2C address of the DDC/CI command channel.
pub const DDC_CHIP: u8 = 0x37;
/// 7-bit I2C address of the EDID EEPROM.
pub const EDID_CHIP: u8 = 0x50;
/// Source address byte at the head of every host frame.
pub const DDC_SRC: u8 = 0x51;
/// 8-bit write address of the display. Seeds request checksums and opens every reply.
const DISPLAY_ADDR: u8 = DDC_CHIP << 1;
/// Host address. Seeds reply checksums, since the display is talking to the host.
const HOST_ADDR: u8 = 0x50;

const OP_GET: u8 = 0x01;
const OP_GET_REPLY: u8 = 0x02;
const OP_SET: u8 = 0x03;
const OP_CAPS_REPLY: u8 = 0xE3;
const OP_CAPS: u8 = 0xF3;

/// Length byte of every get reply: 8 payload bytes.
const GET_REPLY_LEN: u8 = 0x88;
/// Length byte of the null message a busy or unwilling display sends.
const NULL_LEN: u8 = 0x80;

/// Whether a VCP code is a continuous or non-continuous (lookup) value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Continuous,
    NonContinuous,
}

/// A decoded "get VCP feature" reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub vcp: u8,
    pub kind: ValueKind,
    pub max: u16,
    pub current: u16,
}

/// Why a reply frame was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    /// The display answered with the DDC null message (0x6E 0x80 0xBE).
    NullMessage,
    BadSource(u8),
    /// The length byte is not what this reply type always carries.
    BadLength(u8),
    BadOpcode(u8),
    /// Display reported an error result code for the requested feature.
    Unsupported(u8),
    /// Reply echoed a different VCP code than requested.
    Mismatch {
        expected: u8,
        got: u8,
    },
    BadChecksum {
        expected: u8,
        got: u8,
    },
    /// Capability fragment for a different offset than requested.
    BadOffset {
        expected: u16,
        got: u16,
    },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "reply too short"),
            DecodeError::NullMessage => write!(f, "display sent the null message"),
            DecodeError::BadSource(b) => {
                write!(f, "reply from 0x{b:02X}, expected 0x{DISPLAY_ADDR:02X}")
            }
            DecodeError::BadLength(b) => write!(f, "unexpected length byte 0x{b:02X}"),
            DecodeError::BadOpcode(b) => write!(f, "unexpected opcode 0x{b:02X}"),
            DecodeError::Unsupported(r) => write!(f, "display refused the code (result 0x{r:02X})"),
            DecodeError::Mismatch { expected, got } => {
                write!(f, "asked for 0x{expected:02X}, reply is for 0x{got:02X}")
            }
            DecodeError::BadChecksum { expected, got } => {
                write!(
                    f,
                    "bad checksum: got 0x{got:02X}, computed 0x{expected:02X}"
                )
            }
            DecodeError::BadOffset { expected, got } => {
                write!(f, "caps fragment for offset {got}, asked for {expected}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

fn xor(seed: u8, bytes: &[u8]) -> u8 {
    bytes.iter().fold(seed, |ck, b| ck ^ b)
}

fn build(payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 2);
    f.push(0x80 | payload.len() as u8);
    f.extend_from_slice(payload);
    f.push(xor(DISPLAY_ADDR ^ DDC_SRC, &f));
    f
}

/// Frame requesting the current value of `vcp`.
pub fn encode_get(vcp: u8) -> Vec<u8> {
    build(&[OP_GET, vcp])
}

/// Frame setting `vcp` to `value`.
pub fn encode_set(vcp: u8, value: u16) -> Vec<u8> {
    let [hi, lo] = value.to_be_bytes();
    build(&[OP_SET, vcp, hi, lo])
}

/// Frame requesting a fragment of the capabilities string at `offset`.
pub fn encode_caps(offset: u16) -> Vec<u8> {
    let [hi, lo] = offset.to_be_bytes();
    build(&[OP_CAPS, hi, lo])
}

/// Check address, null message and checksum. Returns the frame's length byte.
///
/// Runs before any payload byte is read, so a corrupted byte can't pass as a
/// real result code.
fn check_envelope(buf: &[u8]) -> Result<usize, DecodeError> {
    if buf.len() < 3 {
        return Err(DecodeError::TooShort);
    }
    if buf[0] != DISPLAY_ADDR {
        return Err(DecodeError::BadSource(buf[0]));
    }
    if buf[1] == NULL_LEN {
        return Err(DecodeError::NullMessage);
    }
    if buf[1] & 0x80 == 0 {
        return Err(DecodeError::BadLength(buf[1]));
    }
    let len = (buf[1] & 0x7F) as usize;
    let ck = 2 + len;
    if ck >= buf.len() {
        return Err(DecodeError::TooShort);
    }
    let computed = xor(HOST_ADDR, &buf[..ck]);
    if buf[ck] != computed {
        return Err(DecodeError::BadChecksum {
            expected: computed,
            got: buf[ck],
        });
    }
    Ok(len)
}

/// Decode a reply to [`encode_get`].
pub fn decode_get_reply(buf: &[u8], expected: u8) -> Result<Reply, DecodeError> {
    if buf.len() < 11 {
        return Err(DecodeError::TooShort);
    }
    if buf[0] != DISPLAY_ADDR {
        return Err(DecodeError::BadSource(buf[0]));
    }
    if buf[1] == NULL_LEN {
        return Err(DecodeError::NullMessage);
    }
    if buf[1] != GET_REPLY_LEN {
        return Err(DecodeError::BadLength(buf[1]));
    }
    check_envelope(buf)?;
    if buf[2] != OP_GET_REPLY {
        return Err(DecodeError::BadOpcode(buf[2]));
    }
    if buf[3] != 0x00 {
        return Err(DecodeError::Unsupported(buf[3]));
    }
    if buf[4] != expected {
        return Err(DecodeError::Mismatch {
            expected,
            got: buf[4],
        });
    }
    Ok(Reply {
        vcp: buf[4],
        kind: if buf[5] == 0 {
            ValueKind::Continuous
        } else {
            ValueKind::NonContinuous
        },
        max: u16::from_be_bytes([buf[6], buf[7]]),
        current: u16::from_be_bytes([buf[8], buf[9]]),
    })
}

/// Decode one capabilities fragment into `(echoed offset, ASCII bytes)`.
///
/// The caller should check the offset matches what it asked for; a display
/// that answers a stale request would otherwise splice text in the wrong place.
pub fn decode_caps_fragment(buf: &[u8]) -> Result<(u16, &[u8]), DecodeError> {
    let len = check_envelope(buf)?;
    if buf[2] != OP_CAPS_REPLY {
        return Err(DecodeError::BadOpcode(buf[2]));
    }
    if len < 3 {
        return Err(DecodeError::BadLength(buf[1]));
    }
    let offset = u16::from_be_bytes([buf[3], buf[4]]);
    Ok((offset, &buf[5..2 + len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from Dell's own software talking to a U4323QE.
    const BRIGHTNESS: [u8; 12] = [
        0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x36, 0xF6, 0x3A,
    ];
    const INPUT: [u8; 12] = [
        0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x0E, 0x1B, 0x1B, 0xDA, 0x2F,
    ];

    fn caps_frame(offset: u16, text: &[u8]) -> Vec<u8> {
        let [hi, lo] = offset.to_be_bytes();
        let mut f = vec![0x6E, 0x80 | (text.len() as u8 + 3), 0xE3, hi, lo];
        f.extend_from_slice(text);
        f.push(xor(HOST_ADDR, &f));
        f
    }

    #[test]
    fn matches_captured_get_frames() {
        assert_eq!(encode_get(0xF1), vec![0x82, 0x01, 0xF1, 0x4D]);
        assert_eq!(encode_get(0x10), vec![0x82, 0x01, 0x10, 0xAC]);
        assert_eq!(encode_get(0x12), vec![0x82, 0x01, 0x12, 0xAE]);
        assert_eq!(encode_get(0xE2), vec![0x82, 0x01, 0xE2, 0x5E]);
        assert_eq!(encode_get(0xE9), vec![0x82, 0x01, 0xE9, 0x55]);
    }

    #[test]
    fn matches_captured_set_frames() {
        // Dell's software turning PiP on: 0xE9 = 0x0021.
        assert_eq!(
            encode_set(0xE9, 0x0021),
            vec![0x84, 0x03, 0xE9, 0x00, 0x21, 0x70]
        );
        assert_eq!(
            encode_set(0x02, 0x0001),
            vec![0x84, 0x03, 0x02, 0x00, 0x01, 0xBB]
        );
    }

    #[test]
    fn decodes_captured_replies() {
        let r = decode_get_reply(&BRIGHTNESS, 0x10).unwrap();
        assert_eq!((r.max, r.current, r.kind), (100, 54, ValueKind::Continuous));
        let r = decode_get_reply(&INPUT, 0x60).unwrap();
        assert_eq!(r.current, 0x1B1B);
    }

    #[test]
    fn checksum_catches_a_flipped_bit_anywhere() {
        for i in 3..10 {
            let mut bad = BRIGHTNESS;
            bad[i] ^= 0x01;
            assert!(
                matches!(
                    decode_get_reply(&bad, 0x10),
                    Err(DecodeError::BadChecksum { .. })
                ),
                "byte {i}"
            );
        }
    }

    #[test]
    fn rejects_a_garbled_length_byte() {
        let mut bad = BRIGHTNESS;
        bad[1] = 0x89;
        assert_eq!(
            decode_get_reply(&bad, 0x10),
            Err(DecodeError::BadLength(0x89))
        );
    }

    #[test]
    fn detects_null_message() {
        assert_eq!(
            decode_get_reply(&[0x6E, 0x80, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0], 0x10),
            Err(DecodeError::NullMessage)
        );
    }

    #[test]
    fn decodes_a_caps_fragment_with_its_offset() {
        let f = caps_frame(0x0120, b"vcp(02 04)");
        assert_eq!(decode_caps_fragment(&f), Ok((0x0120, &b"vcp(02 04)"[..])));
        assert_eq!(
            decode_caps_fragment(&caps_frame(64, b"")),
            Ok((64, &[][..]))
        );
    }

    #[test]
    fn caps_fragment_checksum_is_verified() {
        let mut f = caps_frame(0, b"prot(monitor)");
        f[7] ^= 0x20;
        assert!(matches!(
            decode_caps_fragment(&f),
            Err(DecodeError::BadChecksum { .. })
        ));
    }
}
