//! Transport layer: an `I2c` byte-pipe trait plus the DDC session that drives it.
//!
//! The protocol lives in `ddc-core` and is platform-independent. Only the
//! implementations of [`I2c`] are OS-specific.

use std::collections::HashMap;
use std::time::Duration;

use ddc_core::vcp::{default_panel, for_model};
use ddc_core::{
    decode_caps_fragment, decode_get_reply, encode_caps, encode_get, encode_set, Capabilities,
    DecodeError, Panel, Reply, DDC_CHIP, DDC_SRC, EDID_CHIP,
};

#[cfg(target_os = "macos")]
pub mod macos;

pub mod runner;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use runner::Runner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The underlying I2C call failed. `code` is the platform return value.
    Io { op: &'static str, code: i32 },
    /// The panel understood the request and declined it. On this hardware the
    /// result byte is 0x01 over DDC and 0xFE over the USB-HID tunnel.
    Refused { vcp: u8, result: u8 },
    /// Every retry was exhausted without a usable reply.
    NoReply { vcp: u8, attempts: u32 },
    /// No external display exposing a DDC-capable service.
    NotFound,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io { op, code } => write!(f, "i2c {op} failed: 0x{code:08X}"),
            Error::Refused { vcp, result } => {
                write!(f, "panel declined VCP 0x{vcp:02X} (result 0x{result:02X})")
            }
            Error::NoReply { vcp, attempts } => {
                write!(f, "no reply for VCP 0x{vcp:02X} after {attempts} attempts")
            }
            Error::NotFound => write!(f, "no external display found"),
        }
    }
}
impl std::error::Error for Error {}

/// A raw I2C byte pipe to one display.
pub trait I2c {
    fn write(&mut self, chip: u8, offset: u8, data: &[u8]) -> Result<(), Error>;
    fn read(&mut self, chip: u8, offset: u8, out: &mut [u8]) -> Result<(), Error>;

    /// Re-establish the underlying connection.
    ///
    /// Input and PiP/PBP changes make the panel re-sync, which kills a cached
    /// macOS `IOAVService` handle. The default is a no-op.
    fn reconnect(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Timing and retry behaviour. Defaults mirror what Dell's own software does.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Send every request frame twice. A U4323QE answers a single write with
    /// the DDC Null Message nearly every time.
    pub double_write: bool,
    /// Wait before the first write.
    pub pre: Duration,
    /// Wait between the two writes of a double write.
    pub between: Duration,
    /// Wait between the last write and the read.
    pub reply: Duration,
    /// Attempts per read (a get, or one capabilities fragment).
    pub retries: u32,
    /// Apply the panel's per-code settle time around sets. The next frame
    /// arriving too early loses the write.
    pub settle: bool,
    /// Extra whole reads of the capabilities string after it goes unanswered.
    /// A panel re-syncing after an input or layout change stays quiet for a
    /// second or so.
    pub caps_rereads: u32,
    /// Wait before the first of those rereads; it doubles each time.
    pub caps_backoff: Duration,
    /// Gap between [`Ddc::sample_mode`]'s reads, so they span a few
    /// input-scan dwells.
    pub sample_gap: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            double_write: true,
            pre: Duration::from_millis(40),
            between: Duration::from_millis(40),
            reply: Duration::from_millis(40),
            retries: 4,
            settle: true,
            // 0.2 + 0.4 + 0.8 + 1.6: about 3 s of waiting before giving up.
            caps_rereads: 4,
            caps_backoff: Duration::from_millis(200),
            sample_gap: Duration::from_millis(120),
        }
    }
}

/// Reads taken by [`Ddc::sample_mode`].
const SAMPLES: u32 = 9;
/// No real capabilities string is this long; stop paging if a panel says otherwise.
const MAX_CAPS_LEN: usize = 4096;
/// The capabilities request opcode, used as the "vcp" in errors about it.
const CAPS_OPCODE: u8 = 0xF3;

/// A DDC/CI session over any [`I2c`] transport.
pub struct Ddc<T: I2c> {
    pub transport: T,
    pub policy: Policy,
    /// Where names and write quirks come from. See [`Ddc::detect_panel`].
    panel: &'static Panel,
    /// Set once this operation has reconnected, so a dead handle costs one
    /// reconnect wait per get/set rather than one per I/O call.
    reconnected: bool,
}

impl<T: I2c> Ddc<T> {
    pub fn new(transport: T) -> Self {
        Self::with_policy(transport, Policy::default())
    }

    /// A session using the default profile until [`detect_panel`](Ddc::detect_panel)
    /// or [`with_panel`](Ddc::with_panel) says otherwise. Sends nothing.
    pub fn with_policy(transport: T, policy: Policy) -> Self {
        Ddc {
            transport,
            policy,
            panel: default_panel(),
            reconnected: false,
        }
    }

    /// Use `panel`'s profile instead.
    pub fn with_panel(mut self, panel: &'static Panel) -> Self {
        self.panel = panel;
        self
    }

    /// The profile this session uses.
    pub fn panel(&self) -> &'static Panel {
        self.panel
    }

    /// Read the EDID and switch to the profile covering its model.
    ///
    /// Returns the model when the EDID was readable. The profile stays as it
    /// was when no profile covers that model, since another model's quirks can
    /// wedge a panel; compare with [`panel`](Ddc::panel) to tell.
    pub fn detect_panel(&mut self) -> Option<String> {
        let bytes = self.edid().ok()?;
        let model = ddc_core::edid::parse(&bytes).ok()?.model();
        if let Some(p) = for_model(&model) {
            self.panel = p;
        }
        Some(model)
    }

    /// Run one I/O call, reconnecting and retrying once if it fails and this
    /// operation has not reconnected yet.
    fn with_reconnect(
        &mut self,
        mut op: impl FnMut(&mut T) -> Result<(), Error>,
    ) -> Result<(), Error> {
        match op(&mut self.transport) {
            Err(first) if !self.reconnected => {
                self.reconnected = true;
                if self.transport.reconnect().is_err() {
                    return Err(first);
                }
                op(&mut self.transport)
            }
            r => r,
        }
    }

    fn send(&mut self, frame: &[u8]) -> Result<(), Error> {
        std::thread::sleep(self.policy.pre);
        self.with_reconnect(|t| t.write(DDC_CHIP, DDC_SRC, frame))?;
        if self.policy.double_write {
            std::thread::sleep(self.policy.between);
            self.with_reconnect(|t| t.write(DDC_CHIP, DDC_SRC, frame))?;
        }
        Ok(())
    }

    fn exchange(&mut self, frame: &[u8], out: &mut [u8]) -> Result<(), Error> {
        self.send(frame)?;
        std::thread::sleep(self.policy.reply);
        out.fill(0);
        self.with_reconnect(|t| t.read(DDC_CHIP, 0x00, out))
    }

    /// Send `frame` and decode the reply, retrying per the policy.
    ///
    /// A refusal returns at once. If no attempt got as far as a reply, the
    /// last transport error comes back instead of `NoReply`.
    fn request<R>(
        &mut self,
        vcp: u8,
        frame: &[u8],
        out: &mut [u8],
        decode: impl Fn(&[u8]) -> Result<R, DecodeError>,
    ) -> Result<R, Error> {
        let mut io = None;
        let mut replied = false;
        for _ in 0..self.policy.retries {
            if let Err(e) = self.exchange(frame, out) {
                io = Some(e);
                continue;
            }
            replied = true;
            match decode(out) {
                Ok(r) => return Ok(r),
                Err(DecodeError::Unsupported(result)) => {
                    return Err(Error::Refused { vcp, result })
                }
                Err(_) => {}
            }
        }
        match io {
            Some(e) if !replied => Err(e),
            _ => Err(Error::NoReply {
                vcp,
                attempts: self.policy.retries,
            }),
        }
    }

    /// Read the current value of a VCP feature.
    pub fn get(&mut self, vcp: u8) -> Result<Reply, Error> {
        self.reconnected = false;
        let mut buf = [0u8; 12];
        self.request(vcp, &encode_get(vcp), &mut buf, |b| {
            decode_get_reply(b, vcp)
        })
    }

    /// Best-effort read while Auto Select is cycling inputs.
    ///
    /// Takes several reads over a few scan dwells and returns the most common
    /// value. A panel that dwells evenly can still fool it; the real fix is
    /// turning Auto Select off in the OSD.
    pub fn sample_mode(&mut self, vcp: u8) -> Result<Reply, Error> {
        // value -> (count, first seen, reply); first seen breaks ties.
        let mut seen: HashMap<u16, (u32, u32, Reply)> = HashMap::new();
        let mut err = None;
        for i in 0..SAMPLES {
            match self.get(vcp) {
                Ok(r) => seen.entry(r.current).or_insert((0, i, r)).0 += 1,
                Err(e) => err = Some(e),
            }
            if i + 1 < SAMPLES {
                std::thread::sleep(self.policy.sample_gap);
            }
        }
        seen.into_values()
            .max_by_key(|(n, first, _)| (*n, std::cmp::Reverse(*first)))
            .map(|(_, _, r)| r)
            .ok_or_else(|| {
                err.unwrap_or(Error::NoReply {
                    vcp,
                    attempts: SAMPLES,
                })
            })
    }

    /// Write a VCP feature. DDC sets are unacknowledged; verify with `get` if needed.
    ///
    /// Sleeps the code's settle time afterwards, and before too for the slow
    /// codes, since Dell's software sleeps before and it costs little to do both.
    pub fn set(&mut self, vcp: u8, value: u16) -> Result<(), Error> {
        self.reconnected = false;
        let (slow, settle) = if self.policy.settle {
            let q = &self.panel.quirks;
            let slow = q.slow_write_codes.contains(&vcp);
            let ms = if slow {
                q.slow_write_ms
            } else {
                q.normal_write_ms
            };
            (slow, Duration::from_millis(ms))
        } else {
            (false, Duration::ZERO)
        };
        if slow {
            std::thread::sleep(settle);
        }
        self.send(&encode_set(vcp, value))?;
        std::thread::sleep(settle);
        Ok(())
    }

    /// Read and parse the capabilities string.
    ///
    /// If the panel goes quiet (no reply or an I/O error) the whole read starts
    /// again after a backoff, up to [`Policy::caps_rereads`] times, which rides
    /// out the re-sync after a layout or input change. Still fails if it never
    /// answers, and never parses a truncated string.
    pub fn capabilities(&mut self) -> Result<Capabilities, Error> {
        let mut wait = self.policy.caps_backoff;
        let mut left = self.policy.caps_rereads;
        loop {
            match self.read_caps() {
                Err(Error::NoReply { .. } | Error::Io { .. }) if left > 0 => {
                    left -= 1;
                    std::thread::sleep(wait);
                    wait *= 2;
                }
                r => return r,
            }
        }
    }

    /// One pass over the capabilities fragments, up to the empty terminator.
    fn read_caps(&mut self) -> Result<Capabilities, Error> {
        self.reconnected = false;
        let mut raw = Vec::new();
        loop {
            let offset = raw.len() as u16;
            let mut buf = [0u8; 40];
            let frag = self.request(CAPS_OPCODE, &encode_caps(offset), &mut buf, |b| {
                // A fragment for the wrong offset counts as a bad reply and is retried.
                match decode_caps_fragment(b)? {
                    (at, f) if at == offset => Ok(f.to_vec()),
                    (at, _) => Err(DecodeError::BadOffset {
                        expected: offset,
                        got: at,
                    }),
                }
            })?;
            if frag.is_empty() {
                break;
            }
            raw.extend_from_slice(&frag);
            if raw.len() > MAX_CAPS_LEN {
                break;
            }
        }
        Ok(Capabilities::parse(&String::from_utf8_lossy(&raw)))
    }

    /// Read the 128-byte EDID block. Useful for identifying the panel.
    pub fn edid(&mut self) -> Result<[u8; 128], Error> {
        let mut buf = [0u8; 128];
        self.transport.read(EDID_CHIP, 0x00, &mut buf)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::testing::{caps_replies, edid_block, fast, DeadI2c, EdidPanel, ReplayI2c};

    /// Bytes lifted verbatim from a capture of Dell's own software.
    const BRIGHTNESS: [u8; 12] = [
        0x6E, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x36, 0xF6, 0x3A,
    ];
    const INPUT: [u8; 12] = [
        0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x0E, 0x1B, 0x1B, 0xDA, 0x2F,
    ];
    const NULL_MSG: [u8; 12] = [0x6E, 0x80, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    /// A get reply with result byte 0x01: the panel declines the code.
    const REFUSED: [u8; 12] = [
        0x6E, 0x88, 0x02, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0xA5, 0x00,
    ];

    #[test]
    fn decodes_a_real_captured_reply() {
        let mut d = fast(ReplayI2c::new(vec![BRIGHTNESS.to_vec()]));
        let r = d.get(0x10).unwrap();
        assert_eq!((r.current, r.max), (54, 100));
    }

    #[test]
    fn every_request_is_written_twice() {
        // One write is answered with the Null Message; pin the double write.
        let mut d = fast(ReplayI2c::new(vec![BRIGHTNESS.to_vec()]));
        d.get(0x10).unwrap();
        assert_eq!(d.transport.writes.len(), 2);
        assert_eq!(d.transport.writes[0], d.transport.writes[1]);
        assert_eq!(d.transport.logical_writes().len(), 1);
    }

    #[test]
    fn single_write_can_be_disabled() {
        let t = ReplayI2c::new(vec![BRIGHTNESS.to_vec()]);
        let policy = Policy {
            double_write: false,
            ..crate::testing::instant()
        };
        let mut d = Ddc::with_policy(t, policy);
        d.get(0x10).unwrap();
        assert_eq!(d.transport.writes.len(), 1);
    }

    #[test]
    fn null_message_is_retried_then_reported() {
        let mut d = fast(ReplayI2c::new(vec![NULL_MSG.to_vec(); 2]));
        assert!(matches!(d.get(0x10), Err(Error::NoReply { vcp: 0x10, .. })));
        assert_eq!(
            d.transport.writes.len(),
            2 * Policy::default().retries as usize
        );
    }

    #[test]
    fn recovers_when_the_first_attempt_is_a_null_message() {
        let mut d = fast(ReplayI2c::new(vec![NULL_MSG.to_vec(), BRIGHTNESS.to_vec()]));
        assert_eq!(d.get(0x10).unwrap().current, 54);
    }

    #[test]
    fn a_refusal_returns_at_once() {
        let mut d = fast(ReplayI2c::new(vec![NULL_MSG.to_vec(), REFUSED.to_vec()]));
        assert_eq!(
            d.get(0x10),
            Err(Error::Refused {
                vcp: 0x10,
                result: 0x01
            })
        );
        // Two attempts, not four: no point asking again after a definite no.
        assert_eq!(d.transport.logical_writes().len(), 1);
        assert_eq!(d.transport.writes.len(), 4);
    }

    #[test]
    fn mismatched_reply_code_is_not_accepted() {
        // Reply carries 0x60 while 0x10 was asked for.
        let mut d = fast(ReplayI2c::new(vec![INPUT.to_vec()]));
        assert!(d.get(0x10).is_err());
    }

    #[test]
    fn a_dead_transport_reports_the_io_error() {
        let mut d = fast(DeadI2c);
        assert!(matches!(d.get(0x10), Err(Error::Io { .. })));
    }

    #[test]
    fn a_dead_transport_reconnects_once_per_get() {
        let mut d = fast(ReplayI2c::panel().failing());
        assert!(d.get(0x10).is_err());
        assert_eq!(d.transport.reconnects, 1);
        assert!(d.get(0x10).is_err());
        assert_eq!(d.transport.reconnects, 2);
    }

    #[test]
    fn set_writes_twice_and_does_not_read() {
        let mut d = fast(ReplayI2c::new(vec![]));
        d.set(0xE9, 0x0024).unwrap();
        assert_eq!(d.transport.writes.len(), 2);
        assert_eq!(
            d.transport.writes[0],
            vec![0x84, 0x03, 0xE9, 0x00, 0x24, 0x75]
        );
    }

    #[test]
    fn capabilities_are_paged_to_the_terminator() {
        let raw = ddc_core::fixture::U4323QE;
        let mut d = fast(ReplayI2c::new(caps_replies(raw)));
        assert_eq!(d.capabilities().unwrap().raw, raw);
    }

    #[test]
    fn a_lost_caps_fragment_is_an_error_not_a_short_string() {
        // First fragment arrives, then only Null Messages.
        let mut replies = caps_replies(ddc_core::fixture::U4323QE);
        replies.truncate(1);
        let mut d = fast(ReplayI2c::new(replies));
        assert!(matches!(
            d.capabilities(),
            Err(Error::NoReply { vcp: 0xF3, .. })
        ));
    }

    #[test]
    fn caps_ride_out_a_resync() {
        // Silent for two whole reads and a bit, as after a 0xE9 write.
        let silent = 2 * Policy::default().retries + 1;
        let raw = ddc_core::fixture::U4323QE;
        let mut d = fast(ReplayI2c::panel().with_caps(raw).silent_caps(silent));
        assert_eq!(d.capabilities().unwrap().raw, raw);

        // Silent for longer than the rereads cover: still an error.
        let rereads = Policy::default().caps_rereads;
        let silent = (rereads + 1) * Policy::default().retries;
        let mut d = fast(ReplayI2c::panel().with_caps(raw).silent_caps(silent));
        assert!(matches!(
            d.capabilities(),
            Err(Error::NoReply { vcp: 0xF3, .. })
        ));
    }

    #[test]
    fn a_dead_caps_read_is_reread_then_reported() {
        let mut d = fast(ReplayI2c::panel().failing());
        assert!(matches!(d.capabilities(), Err(Error::Io { .. })));
        // one reconnect per whole read: the first plus each reread
        assert_eq!(d.transport.reconnects, Policy::default().caps_rereads + 1);
    }

    #[test]
    fn each_session_takes_its_own_profile_from_edid() {
        let open = |model: &str| {
            let edid = edid_block(model, "SN1");
            let mut d = fast(EdidPanel::new(ReplayI2c::panel(), edid));
            let read = d.detect_panel();
            (d, read)
        };
        let (a, _) = open("DELL U2723QE");
        let (b, _) = open("DELL U4323QE");
        assert_eq!(a.panel().model, "DELL U2723QE");
        assert_eq!(b.panel().model, "DELL U4323QE");

        // An unknown model is reported and keeps the default.
        let (c, read) = open("DELL NOSUCH");
        assert_eq!(read.as_deref(), Some("DELL NOSUCH"));
        assert_eq!(c.panel(), ddc_core::vcp::default_panel());

        // No EDID: nothing read, default kept.
        let mut d = fast(EdidPanel::new(ReplayI2c::panel(), Vec::new()));
        assert_eq!(d.detect_panel(), None);
        assert_eq!(d.panel(), ddc_core::vcp::default_panel());
    }

    #[test]
    fn with_panel_overrides_the_default() {
        let p = ddc_core::vcp::for_model("DELL U2723QE").unwrap();
        let d = fast(ReplayI2c::new(vec![])).with_panel(p);
        assert_eq!(d.panel(), p);
    }
}
