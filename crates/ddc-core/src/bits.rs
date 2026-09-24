//! Decoders for Dell's packed status words (0x60, 0xF2, 0xF1, 0xE9).
//! Pure functions over a u16. Each is tested against a word read off a U4323QE.

use crate::vcp::pip;

// ---------------------------------------------------------------------------
// VCP 0x60: input source
// ---------------------------------------------------------------------------

/// Lowest USB-DDC pseudo-port code seen in 0x60's high byte.
pub const USB_DDC_MIN: u8 = 0x80;
/// Highest USB-DDC pseudo-port code DDPM's validator accepts.
pub const USB_DDC_MAX: u8 = 0x85;
/// Highest input code DDPM treats as valid in either byte of 0x60.
pub const MAX_INPUT_CODE: u8 = 0x1E;

/// Where a DDC request arrived from, per VCP 0x60's high byte.
///
/// Captured from a U4323QE: over video ports the high byte is always a real
/// input code, and `0x8x` only shows up over the USB-HID tunnel, so it isn't
/// just a copy of the low byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// This request came in over a video port; the value is a 0x60 input code.
    Port(u8),
    /// This request came in over the USB-HID tunnel (0x80..=0x85).
    UsbTunnel(u8),
    /// High byte is zero: the panel did not report an arrival port.
    Unreported,
    /// Outside every range DDPM's validator accepts.
    Invalid(u8),
}

impl Arrival {
    /// The raw high byte, whatever it meant.
    pub fn raw(self) -> u8 {
        match self {
            Arrival::Port(v) | Arrival::UsbTunnel(v) | Arrival::Invalid(v) => v,
            Arrival::Unreported => 0,
        }
    }

    /// Which upstream a USB-DDC pseudo-port names. Inferred from DDPM, not measured.
    pub fn usb_tunnel_name(self) -> Option<&'static str> {
        match self {
            Arrival::UsbTunnel(0x80) => Some("usb-ddc-b1"),
            Arrival::UsbTunnel(0x81) => Some("usb-ddc-b2"),
            Arrival::UsbTunnel(0x82) => Some("usb-ddc-c1"),
            Arrival::UsbTunnel(0x83) => Some("usb-ddc-c2"),
            Arrival::UsbTunnel(0x84) => Some("usb-ddc-c3"),
            Arrival::UsbTunnel(0x85) => Some("usb-ddc-c4"),
            _ => None,
        }
    }
}

/// A decoded VCP 0x60 word: both halves, kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputWord {
    pub raw: u16,
    /// High byte: the port the DDC request arrived on.
    pub arrival: Arrival,
    /// Low byte: the input the **main window** is currently showing.
    pub selected: u8,
}

/// Decode VCP 0x60. Never masks the high byte away.
pub fn input_word(raw: u16) -> InputWord {
    let hh = (raw >> 8) as u8;
    let ll = (raw & 0xFF) as u8;
    let arrival = match hh {
        0 => Arrival::Unreported,
        v if (USB_DDC_MIN..=USB_DDC_MAX).contains(&v) => Arrival::UsbTunnel(v),
        v if v <= MAX_INPUT_CODE => Arrival::Port(v),
        v => Arrival::Invalid(v),
    };
    InputWord {
        raw,
        arrival,
        selected: ll,
    }
}

impl InputWord {
    /// True when the host that issued this request owns the main window.
    ///
    /// `None` when the arrival port wasn't reported or is a USB pseudo-port,
    /// which can't be compared against an input code.
    pub fn asker_owns_main(self) -> Option<bool> {
        match self.arrival {
            Arrival::Port(p) => Some(p == self.selected),
            _ => None,
        }
    }

    /// DDPM's validity check: HH <= 0x85 and LL <= 0x1E. Discard a read that fails it.
    ///
    /// Looser than [`Arrival`] on purpose: HH in 0x1F..=0x7F passes here but is
    /// neither an input nor a pseudo-port, so it's plausible but not interpretable.
    pub fn is_valid(self) -> bool {
        let hh = (self.raw >> 8) as u8;
        hh <= USB_DDC_MAX && self.selected <= MAX_INPUT_CODE
    }
}

/// Why a 0x60 sample was thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleReject {
    /// High byte above 0x85: neither an arrival port nor a USB pseudo-port.
    ArrivalByte(u8),
    /// Low byte outside 0x01..=0x1E: names no input. `0x00` lands here too.
    SelectedByte(u8),
}

impl std::fmt::Display for SampleReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            SampleReject::ArrivalByte(v) => {
                write!(
                    f,
                    "high byte 0x{v:02X} is above 0x85, so it isn't an arrival port"
                )
            }
            SampleReject::SelectedByte(v) => {
                write!(
                    f,
                    "low byte 0x{v:02X} is outside 0x01..0x1E, so it names no input"
                )
            }
        }
    }
}

/// Accept a 0x60 reading only if it's structurally possible.
///
/// While Auto Select hunts, a read can catch a half-formed word. Stricter than
/// [`InputWord::is_valid`]: a zero low byte is a legal answer but a useless
/// sample.
pub fn accept_input_sample(raw: u16) -> Result<InputWord, SampleReject> {
    let hh = (raw >> 8) as u8;
    let ll = (raw & 0xFF) as u8;
    if hh > USB_DDC_MAX {
        return Err(SampleReject::ArrivalByte(hh));
    }
    if !(0x01..=MAX_INPUT_CODE).contains(&ll) {
        return Err(SampleReject::SelectedByte(ll));
    }
    Ok(input_word(raw))
}

// ---------------------------------------------------------------------------
// VCP 0xF2: status
// ---------------------------------------------------------------------------

/// Panel is busy: OSD open or mid-operation. Seen on hardware.
pub const STATUS_BUSY: u16 = 1 << 7;
/// Inferred from DDPM (2025+ panels): brightness locked while HDR is on.
pub const STATUS_HDR_BRIGHTNESS_LOCK: u16 = 1 << 0;
/// Inferred from DDPM (2025+ panels): contrast locked while HDR is on.
pub const STATUS_HDR_CONTRAST_LOCK: u16 = 1 << 9;
/// Inferred from DDPM: iMST active.
pub const STATUS_IMST: u16 = 1 << 8;

/// Bits of 0xF2 whose meaning is established (however weakly).
const STATUS_KNOWN: u16 =
    STATUS_BUSY | STATUS_HDR_BRIGHTNESS_LOCK | STATUS_HDR_CONTRAST_LOCK | STATUS_IMST;

/// Decoded VCP 0xF2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub raw: u16,
}

/// Decode VCP 0xF2.
pub fn status(raw: u16) -> Status {
    Status { raw }
}

impl Status {
    /// The one bit measured on this panel. Gate every write on it.
    pub fn busy(self) -> bool {
        self.raw & STATUS_BUSY != 0
    }
    /// Inferred from DDPM, not measured.
    pub fn hdr_brightness_locked(self) -> bool {
        self.raw & STATUS_HDR_BRIGHTNESS_LOCK != 0
    }
    /// Inferred from DDPM, not measured.
    pub fn hdr_contrast_locked(self) -> bool {
        self.raw & STATUS_HDR_CONTRAST_LOCK != 0
    }
    /// Inferred from DDPM, not measured.
    pub fn imst(self) -> bool {
        self.raw & STATUS_IMST != 0
    }
    /// Set bits with no established meaning. Report them rather than pretend
    /// the word is fully decoded.
    pub fn unknown_bits(self) -> u16 {
        self.raw & !STATUS_KNOWN
    }
}

// ---------------------------------------------------------------------------
// VCP 0xF1: feature bitmask
// ---------------------------------------------------------------------------

/// Gates the PBP-Zoom and Underscan controls (DDPM masks 0xF1 with 0x20).
pub const FEATURE_ZOOM_UNDERSCAN: u16 = 1 << 5;
/// DDPM's allow-list / DDC-CI liveness flag; DDPM disconnects the display if clear.
pub const FEATURE_DDPM_ALLOWED: u16 = 1 << 14;
/// Named alongside bit 14 in DDPM's log literal ("F1.14/15"); no separate mask found.
pub const FEATURE_BIT15: u16 = 1 << 15;

const FEATURE_KNOWN: u16 = FEATURE_ZOOM_UNDERSCAN | FEATURE_DDPM_ALLOWED | FEATURE_BIT15;

/// Decoded VCP 0xF1. Read-only; DDPM never writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features {
    pub raw: u16,
}

/// Decode VCP 0xF1.
pub fn features(raw: u16) -> Features {
    Features { raw }
}

impl Features {
    /// Whether DDPM would show the PBP zoom / underscan controls. Not a
    /// capability gate: it's set on the U4323QE and both actions do nothing
    /// there, so don't refuse a write on it.
    pub fn zoom_underscan(self) -> bool {
        self.raw & FEATURE_ZOOM_UNDERSCAN != 0
    }
    /// DDPM's allow-list flag; doubles as a liveness probe (0xFFFF = read failed).
    pub fn ddpm_allowed(self) -> bool {
        self.raw & FEATURE_DDPM_ALLOWED != 0
    }
    /// 0xFFFF is DDPM's read-failure sentinel for this code, not a real value.
    pub fn is_read_failure(self) -> bool {
        self.raw == 0xFFFF
    }
    /// Set bits with no known meaning. Four of seven on a U4323QE (0xC12B).
    pub fn unknown_bits(self) -> u16 {
        self.raw & !FEATURE_KNOWN
    }
}

// ---------------------------------------------------------------------------
// VCP 0xE9: pane counts
// ---------------------------------------------------------------------------

/// How many windows a 0xE9 layout has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub layout: u8,
    /// Pane count, from Dell's layout table or counted on a U4323QE.
    pub count: Option<u8>,
    /// Whether the count was seen on a U4323QE rather than only read from Dell's table.
    pub measured: bool,
}

/// Panes for a 0xE9 layout value. Write-aliases are folded first.
///
/// U4323QE layout table; other panels' layouts come from Dell's table unmeasured.
pub fn panes(layout: u8) -> Panes {
    let (count, measured) = match pip::canonical(layout) {
        0x00 => (Some(1), true),
        0x21 | 0x22 | 0x24 | 0x2F => (Some(2), true),
        0x31..=0x36 => (Some(3), true),
        0x41 => (Some(4), true),
        0x23 | 0x25..=0x2E | 0x51 => (Some(2), false),
        0x42 => (Some(4), false),
        _ => (None, false),
    };
    Panes {
        layout,
        count,
        measured,
    }
}

impl Panes {
    /// Whether the count was measured on hardware.
    pub fn proven(self) -> bool {
        self.measured
    }

    /// The count a guard should use. Unknown layouts get 4, the most any
    /// Dell layout has, so a guard never refuses what the panel would take.
    pub fn bound(self) -> u8 {
        self.count.unwrap_or(4)
    }

    /// Sub-window slots: every pane but the main one, at most 0xE8's three fields.
    pub fn sub_slots(self) -> u8 {
        self.bound().saturating_sub(1).min(3)
    }

    /// Whether window index `w` (main = 0, sub1 = 1, ...) exists in this layout.
    pub fn has_window(self, w: u8) -> bool {
        w < self.bound()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcp::input;

    #[test]
    fn decodes_captured_input_words() {
        // This host on USB-C, main on USB-C.
        let w = input_word(0x1B1B);
        assert_eq!(w.selected, input::USB_C);
        assert_eq!(w.arrival, Arrival::Port(input::USB_C));
        assert_eq!(w.asker_owns_main(), Some(true));
        assert!(w.is_valid());
    }

    #[test]
    fn high_byte_is_not_a_duplicate() {
        // Request arrived on DP2 while another machine held main on DP1.
        let w = input_word(0x130F);
        assert_eq!(w.arrival, Arrival::Port(input::DP2));
        assert_eq!(w.selected, input::DISPLAY_PORT);
        assert_eq!(w.asker_owns_main(), Some(false));
        assert!(w.is_valid());
    }

    #[test]
    fn usb_tunnel_arrival_is_a_pseudo_port() {
        // Same moment, over the HID tunnel.
        let w = input_word(0x8313);
        assert_eq!(w.arrival, Arrival::UsbTunnel(0x83));
        assert_eq!(w.selected, input::DP2);
        // A pseudo-port cannot be compared against an input code.
        assert_eq!(w.asker_owns_main(), None);
        assert_eq!(w.arrival.usb_tunnel_name(), Some("usb-ddc-c2"));
    }

    #[test]
    fn samples_must_name_an_input() {
        assert_eq!(accept_input_sample(0x1B1B).unwrap().selected, input::USB_C);
        assert_eq!(accept_input_sample(0x8313).unwrap().selected, input::DP2);
        assert_eq!(
            accept_input_sample(0x8613),
            Err(SampleReject::ArrivalByte(0x86))
        );
        assert_eq!(
            accept_input_sample(0x1B40),
            Err(SampleReject::SelectedByte(0x40))
        );
        // Valid as a word, useless as a sample.
        assert!(input_word(0x1B00).is_valid());
        assert_eq!(
            accept_input_sample(0x1B00),
            Err(SampleReject::SelectedByte(0))
        );
        assert!(accept_input_sample(0x8613)
            .unwrap_err()
            .to_string()
            .contains("0x85"));
    }

    #[test]
    fn rejects_structurally_impossible_words() {
        assert!(!input_word(0x8613).is_valid()); // HH > 0x85
        assert!(!input_word(0x1B40).is_valid()); // LL > 0x1E
        assert!(input_word(0x0000).is_valid()); // both halves unreported
        assert_eq!(input_word(0x001B).arrival, Arrival::Unreported);
    }

    #[test]
    fn decodes_captured_status_word() {
        // The only 0xF2 value read here: idle.
        let s = status(0x0000);
        assert!(!s.busy());
        assert_eq!(s.unknown_bits(), 0);
        // And the bit that gates every write.
        assert!(status(0x0080).busy());
        assert!(status(0x00FF).busy());
        // An unknown bit is reported, not silently swallowed.
        assert_eq!(status(0x0400).unknown_bits(), 0x0400);
    }

    #[test]
    fn decodes_captured_feature_word() {
        // The U4323QE's resting value.
        let f = features(0xC12B);
        assert!(f.ddpm_allowed());
        assert!(f.zoom_underscan());
        assert!(!f.is_read_failure());
        // Bits 0, 1, 3, 8 are set and unexplained.
        assert_eq!(f.unknown_bits(), 0x010B);
        assert_eq!(f.unknown_bits().count_ones(), 4);
    }

    #[test]
    fn feature_read_failure_sentinel() {
        assert!(features(0xFFFF).is_read_failure());
        assert!(!features(0x0000).ddpm_allowed());
        assert!(!features(0x0000).zoom_underscan());
    }

    #[test]
    fn pane_counts_fold_write_aliases() {
        // Writing 0xE9 = 0x02 reads back 0x24, so both must count the same.
        assert_eq!(panes(0x02).bound(), panes(0x24).bound());
        assert_eq!(panes(0x01).bound(), panes(0x32).bound());
    }

    #[test]
    fn pane_counts_match_the_hardware() {
        for (layout, n) in [(0x00, 1), (0x21, 2), (0x22, 2), (0x24, 2), (0x2F, 2)] {
            assert_eq!(panes(layout).bound(), n, "0x{layout:02X}");
        }
        for layout in [0x31, 0x32, 0x33, 0x34, 0x35] {
            assert_eq!(panes(layout).bound(), 3, "0x{layout:02X}");
        }
        assert_eq!(panes(0x41).bound(), 4);
        assert_eq!(panes(0x00).sub_slots(), 0);
        assert_eq!(panes(0x41).sub_slots(), 3);
        assert!(pip::CANONICAL.iter().all(|l| panes(*l).proven()));
        assert!(!panes(0x42).proven());
    }

    #[test]
    fn unknown_layout_is_maximally_permissive() {
        let s = panes(0x99);
        assert_eq!(s.count, None);
        assert!(!s.proven());
        assert_eq!(s.bound(), 4);
    }
}
