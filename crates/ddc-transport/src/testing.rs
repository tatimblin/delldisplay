//! Fake [`I2c`] transports for tests. Enabled by `cfg(test)` or the `testing`
//! feature.
//!
//! [`ReplayI2c`] replays captured reply bytes and records every write, so the
//! session layer can be exercised with no monitor attached.

use std::collections::{BTreeMap, VecDeque};

use crate::{Ddc, Error, I2c, Policy};

/// A request frame, decoded back into what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Get(u8),
    Set(u8, u16),
    Caps(u16),
    /// A frame this decoder does not recognise, kept rather than dropped.
    Other,
}

/// Decode a host request frame (`[0x80|len, payload.., ck]`) into an [`Op`].
pub fn decode_request(frame: &[u8]) -> Op {
    match frame {
        [_, 0x01, vcp, _] => Op::Get(*vcp),
        [_, 0x03, vcp, hi, lo, _] => Op::Set(*vcp, u16::from_be_bytes([*hi, *lo])),
        [_, 0xF3, hi, lo, _] => Op::Caps(u16::from_be_bytes([*hi, *lo])),
        _ => Op::Other,
    }
}

/// Build the reply frame a panel would send for a GET, checksum included.
///
/// The reply checksum seeds with the host address 0x50, not `0x6E ^ 0x51`.
pub fn encode_reply(vcp: u8, current: u16, max: u16) -> Vec<u8> {
    let [mh, ml] = max.to_be_bytes();
    let [ch, cl] = current.to_be_bytes();
    let mut f = vec![0x6E, 0x88, 0x02, 0x00, vcp, 0x00, mh, ml, ch, cl];
    let ck = f.iter().fold(0x50u8, |a, b| a ^ b);
    f.push(ck);
    f.push(0x00); // pad to the 12 bytes a real read returns
    f
}

/// The `0xE3` fragment a panel returns for `raw` at `off`: up to 32 bytes, or
/// the empty terminator once `off` reaches the end.
fn caps_fragment(raw: &[u8], off: usize) -> Vec<u8> {
    let chunk = &raw[off.min(raw.len())..][..raw.len().saturating_sub(off).min(32)];
    let mut f = vec![
        0x6E,
        0x80 | (chunk.len() as u8 + 3),
        0xE3,
        (off >> 8) as u8,
        off as u8,
    ];
    f.extend_from_slice(chunk);
    f.push(f.iter().fold(0x50, |ck, b| ck ^ b)); // replies seed the XOR with 0x50
    f
}

/// Split a capability string into the `0xE3` fragments a panel would return,
/// ending with the empty terminator a real panel sends.
pub fn caps_replies(raw: &str) -> Vec<Vec<u8>> {
    let raw = raw.as_bytes();
    let mut out: Vec<Vec<u8>> = (0..raw.len())
        .step_by(32)
        .map(|off| caps_fragment(raw, off))
        .collect();
    out.push(caps_fragment(raw, raw.len()));
    out
}

/// The DDC Null Message: what a panel sends when it has nothing to say.
const NULL_MESSAGE: [u8; 3] = [0x6E, 0x80, 0xBE];

/// Replays a scripted sequence of replies, recording every write.
///
/// Once the script runs out, GETs are answered from a fake register file
/// seeded with [`on_get`](Self::on_get), capability requests from
/// [`with_caps`](Self::with_caps), and anything else gets the Null Message.
pub struct ReplayI2c {
    /// Every frame the session wrote, in order.
    pub writes: Vec<Vec<u8>>,
    replies: VecDeque<Vec<u8>>,
    /// `vcp -> (current, max)`.
    values: BTreeMap<u8, (u16, u16)>,
    caps: Option<Vec<u8>>,
    /// A `Set` updates the register file. Turn off to model action registers
    /// (0xE5, 0xE7 = 0xFF00) or a dropped write.
    pub echo_writes: bool,
    /// Make every read and write fail with an I/O error.
    pub fail: bool,
    /// How many times [`I2c::reconnect`] was called.
    pub reconnects: u32,
    /// Answer this many capability reads with the Null Message first.
    pub silent_caps: u32,
}

impl ReplayI2c {
    pub fn new(replies: Vec<Vec<u8>>) -> Self {
        ReplayI2c {
            writes: Vec::new(),
            replies: replies.into(),
            values: BTreeMap::new(),
            caps: None,
            echo_writes: true,
            fail: false,
            reconnects: 0,
            silent_caps: 0,
        }
    }

    /// No scripted frames; every read comes from the register file.
    pub fn panel() -> Self {
        ReplayI2c::new(Vec::new())
    }

    /// Answer capability requests with `raw`, as many times as asked.
    pub fn with_caps(mut self, raw: &str) -> Self {
        self.caps = Some(raw.as_bytes().to_vec());
        self
    }

    /// Leave the first `n` capability reads unanswered, like a panel
    /// re-syncing after a layout change.
    pub fn silent_caps(mut self, n: u32) -> Self {
        self.silent_caps = n;
        self
    }

    /// Fail every I/O call from now on.
    pub fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    /// Seed one register. Chainable: `ReplayI2c::panel().on_get(0x60, 0x1B1B)`.
    pub fn on_get(self, vcp: u8, current: u16) -> Self {
        self.on_get_max(vcp, current, 0xFFFF)
    }

    /// Seed one register with an explicit `max`, as the reply carries it.
    pub fn on_get_max(mut self, vcp: u8, current: u16, max: u16) -> Self {
        self.values.insert(vcp, (current, max));
        self
    }

    /// What the fake panel now holds for `vcp`.
    pub fn value_of(&self, vcp: u8) -> Option<u16> {
        self.values.get(&vcp).map(|(v, _)| *v)
    }

    /// Frames written with consecutive duplicates collapsed, i.e. ignoring the
    /// double write.
    pub fn logical_writes(&self) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = Vec::new();
        for w in &self.writes {
            if out.last() != Some(w) {
                out.push(w.clone());
            }
        }
        out
    }

    /// Every frame decoded, duplicates kept.
    pub fn raw_ops(&self) -> Vec<Op> {
        self.writes.iter().map(|w| decode_request(w)).collect()
    }

    /// What the session asked for, in order, with the double write folded away.
    pub fn ops(&self) -> Vec<Op> {
        self.logical_writes()
            .iter()
            .map(|w| decode_request(w))
            .collect()
    }

    /// Every `Set`, in order. Repeats are kept.
    pub fn sets(&self) -> Vec<(u8, u16)> {
        self.ops()
            .into_iter()
            .filter_map(|o| match o {
                Op::Set(v, x) => Some((v, x)),
                _ => None,
            })
            .collect()
    }

    /// Every `Get`, in order.
    pub fn gets(&self) -> Vec<u8> {
        self.ops()
            .into_iter()
            .filter_map(|o| match o {
                Op::Get(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    /// The VCP codes touched, in order, reads and writes together.
    pub fn code_order(&self) -> Vec<u8> {
        self.ops()
            .into_iter()
            .filter_map(|o| match o {
                Op::Get(v) | Op::Set(v, _) => Some(v),
                _ => None,
            })
            .collect()
    }

    /// True when every frame was written exactly twice, back to back.
    pub fn every_frame_doubled(&self) -> bool {
        self.writes.len().is_multiple_of(2) && self.writes.chunks(2).all(|p| p[0] == p[1])
    }

    /// Assert the `Set` sequence only, ignoring reads.
    #[track_caller]
    pub fn assert_sets(&self, expected: &[(u8, u16)]) {
        let got = self.sets();
        if got != expected {
            let fmt = |v: &[(u8, u16)]| {
                v.iter()
                    .map(|(c, x)| format!("set 0x{c:02X}=0x{x:04X}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            panic!(
                "set order mismatch\n  expected: {}\n  got:      {}",
                fmt(expected),
                fmt(&got)
            );
        }
    }

    /// Assert these codes were touched in this relative order, anything else
    /// allowed in between.
    #[track_caller]
    pub fn assert_code_order(&self, expected: &[u8]) {
        let got = self.code_order();
        let mut it = got.iter();
        for want in expected {
            if !it.any(|c| c == want) {
                panic!(
                    "0x{want:02X} did not appear in order; codes seen: {}",
                    got.iter()
                        .map(|c| format!("{c:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
    }

    /// Forget recorded writes, keeping the register file.
    pub fn clear_writes(&mut self) {
        self.writes.clear();
    }

    /// Answer the last request from the register file or the caps string.
    fn synthesised(&self) -> Option<Vec<u8>> {
        match decode_request(self.writes.last()?) {
            Op::Get(vcp) => {
                let (current, max) = *self.values.get(&vcp)?;
                Some(encode_reply(vcp, current, max))
            }
            Op::Caps(off) => Some(caps_fragment(self.caps.as_ref()?, off as usize)),
            _ => None,
        }
    }
}

impl I2c for ReplayI2c {
    fn write(&mut self, _chip: u8, _offset: u8, data: &[u8]) -> Result<(), Error> {
        if self.fail {
            return Err(Error::Io {
                op: "write",
                code: -1,
            });
        }
        self.writes.push(data.to_vec());
        if self.echo_writes {
            if let Op::Set(vcp, value) = decode_request(data) {
                self.values.entry(vcp).or_insert((0, 0xFFFF)).0 = value;
            }
        }
        Ok(())
    }

    fn read(&mut self, _chip: u8, _offset: u8, out: &mut [u8]) -> Result<(), Error> {
        if self.fail {
            return Err(Error::Io {
                op: "read",
                code: -1,
            });
        }
        let asked_caps = self
            .writes
            .last()
            .is_some_and(|w| matches!(decode_request(w), Op::Caps(_)));
        if asked_caps && self.silent_caps > 0 {
            self.silent_caps -= 1;
            out.fill(0);
            out[..NULL_MESSAGE.len()].copy_from_slice(&NULL_MESSAGE);
            return Ok(());
        }
        let reply = self
            .replies
            .pop_front()
            .or_else(|| self.synthesised())
            .unwrap_or_else(|| NULL_MESSAGE.to_vec());
        out.fill(0);
        let n = reply.len().min(out.len());
        out[..n].copy_from_slice(&reply[..n]);
        Ok(())
    }

    fn reconnect(&mut self) -> Result<(), Error> {
        self.reconnects += 1;
        Ok(())
    }
}

/// The U4323QE at rest: its capability string and idle registers. Chain
/// `on_get` to vary one register.
pub fn u4323qe() -> ReplayI2c {
    let t = ReplayI2c::panel().with_caps(ddc_core::fixture::U4323QE);
    ddc_core::fixture::IDLE_READS
        .iter()
        .fold(t, |t, (vcp, v)| t.on_get(*vcp, *v))
}

/// An [`I2c`] that fails every call.
pub struct DeadI2c;

impl I2c for DeadI2c {
    fn write(&mut self, _: u8, _: u8, _: &[u8]) -> Result<(), Error> {
        Err(Error::Io {
            op: "dead",
            code: -1,
        })
    }
    fn read(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), Error> {
        Err(Error::Io {
            op: "dead",
            code: -1,
        })
    }
}

/// Wraps a transport so every `Set` frame is recorded but then errors, like
/// a write whose host-side call failed after the frame may have gone out.
pub struct WriteFails<T>(pub T);

impl<T: I2c> I2c for WriteFails<T> {
    fn write(&mut self, chip: u8, offset: u8, data: &[u8]) -> Result<(), Error> {
        self.0.write(chip, offset, data)?;
        match decode_request(data) {
            Op::Set(..) => Err(Error::Io {
                op: "write",
                code: -1,
            }),
            _ => Ok(()),
        }
    }
    fn read(&mut self, chip: u8, offset: u8, out: &mut [u8]) -> Result<(), Error> {
        self.0.read(chip, offset, out)
    }
}

/// A panel with an EDID: DDC traffic goes to `ddc`, reads of the EDID chip
/// (0x50) are answered from `edid`. An empty `edid` fails the read.
pub struct EdidPanel {
    pub ddc: ReplayI2c,
    pub edid: Vec<u8>,
    /// Reads of the EDID chip so far.
    pub edid_reads: u32,
}

impl EdidPanel {
    pub fn new(ddc: ReplayI2c, edid: Vec<u8>) -> Self {
        EdidPanel {
            ddc,
            edid,
            edid_reads: 0,
        }
    }
}

impl I2c for EdidPanel {
    fn write(&mut self, chip: u8, offset: u8, data: &[u8]) -> Result<(), Error> {
        self.ddc.write(chip, offset, data)
    }
    fn read(&mut self, chip: u8, offset: u8, out: &mut [u8]) -> Result<(), Error> {
        if chip != ddc_core::EDID_CHIP {
            return self.ddc.read(chip, offset, out);
        }
        self.edid_reads += 1;
        if self.edid.is_empty() {
            return Err(Error::Io {
                op: "edid",
                code: -1,
            });
        }
        out.fill(0);
        let n = out.len().min(self.edid.len());
        out[..n].copy_from_slice(&self.edid[..n]);
        Ok(())
    }
    fn reconnect(&mut self) -> Result<(), Error> {
        self.ddc.reconnect()
    }
}

/// A 128-byte Dell EDID built from the VESA layout: week 23 of 2023, with
/// `model` and `serial` in the text descriptors.
pub fn edid_block(model: &str, serial: &str) -> Vec<u8> {
    let mut e = [0u8; 128];
    e[..8].copy_from_slice(&ddc_core::edid::MAGIC);
    let id: u16 = (4 << 10) | (5 << 5) | 12; // "DEL"
    e[8..10].copy_from_slice(&id.to_be_bytes());
    e[10..12].copy_from_slice(&0xA0D2u16.to_le_bytes());
    e[16] = 23;
    e[17] = 33; // 1990 + 33
    e[18] = 1;
    e[19] = 4;
    let mut put = |off: usize, tag: u8, s: &str| {
        e[off + 3] = tag;
        for (i, b) in s.bytes().take(13).enumerate() {
            e[off + 5 + i] = b;
        }
        if s.len() < 13 {
            e[off + 5 + s.len()] = 0x0A;
        }
    };
    put(72, 0xFF, serial);
    put(90, 0xFC, model);
    let sum = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    e[127] = 0u8.wrapping_sub(sum);
    e.to_vec()
}

/// A [`Ddc`] with every delay removed. Double write and retries stay real.
pub fn fast<T: I2c>(t: T) -> Ddc<T> {
    Ddc::with_policy(t, instant())
}

/// A [`Policy`] with every delay at zero; tests must not sleep.
pub fn instant() -> Policy {
    Policy {
        pre: std::time::Duration::ZERO,
        between: std::time::Duration::ZERO,
        reply: std::time::Duration::ZERO,
        settle: false,
        caps_backoff: std::time::Duration::ZERO,
        sample_gap: std::time::Duration::ZERO,
        ..Default::default()
    }
}
