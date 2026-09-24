//! Who a panel is: the model+serial key snapshots are tagged with, and the
//! firmware version from the `0xC8` / `0xFD` / `0xC9` group.

use crate::edid::Edid;

// ---------------------------------------------------------------------------
// Monitor key
// ---------------------------------------------------------------------------

/// Model plus serial. Two U4323QEs on one desk need the serial to tell them
/// apart, so an empty serial means unknown and never matches anything.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MonitorKey {
    pub model: String,
    pub serial: String,
}

impl MonitorKey {
    pub fn new(model: impl Into<String>, serial: impl Into<String>) -> Self {
        MonitorKey {
            model: norm(&model.into()),
            serial: norm(&serial.into()),
        }
    }

    /// Build from a parsed EDID. The serial is empty when the panel publishes
    /// none, which makes every later comparison [`KeyMatch::Unknown`].
    pub fn from_edid(e: &Edid) -> Self {
        MonitorKey::new(e.model(), e.serial().unwrap_or_default())
    }

    /// Whether this key can tell one panel from another.
    pub fn is_complete(&self) -> bool {
        !self.model.is_empty() && !self.serial.is_empty()
    }
}

/// `U4323QE/ABC123`, or `U4323QE/unknown-serial`.
impl std::fmt::Display for MonitorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.serial.is_empty() {
            write!(f, "{}/unknown-serial", self.model)
        } else {
            write!(f, "{}/{}", self.model, self.serial)
        }
    }
}

/// How well two monitor keys agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMatch {
    /// Same model, same non-empty serial.
    Exact,
    /// Same model, different serials: a second monitor of the same kind.
    SameModel,
    /// Same model, but at least one side has no serial.
    Unknown,
    /// Different models.
    Different,
}

impl KeyMatch {
    pub fn label(self) -> &'static str {
        match self {
            KeyMatch::Exact => "exact",
            KeyMatch::SameModel => "same-model",
            KeyMatch::Unknown => "unknown-serial",
            KeyMatch::Different => "different-model",
        }
    }
}

/// Compare two keys. Models compare case-insensitively, serials exactly.
pub fn match_keys(a: &MonitorKey, b: &MonitorKey) -> KeyMatch {
    if !a.model.eq_ignore_ascii_case(&b.model) {
        return KeyMatch::Different;
    }
    if a.serial.is_empty() || b.serial.is_empty() {
        return KeyMatch::Unknown;
    }
    if a.serial == b.serial {
        KeyMatch::Exact
    } else {
        KeyMatch::SameModel
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Firmware
// ---------------------------------------------------------------------------

/// The firmware group in DDPM's read order.
pub const FIRMWARE_CODES: [u8; 3] = [0xC8, 0xFD, 0xC9];

/// DDPM's gap between the three firmware reads, in milliseconds.
pub const FIRMWARE_GAP_MS: u64 = 100;

/// DDPM treats this reading as a failed read.
pub const FIRMWARE_READ_FAILURE: u16 = 0xFFFF;

/// Shown when [`Firmware::version`] is `None`.
pub const FIRMWARE_VERSION_UNKNOWN: &str =
    "a read failed, or this scaler's version prefix isn't known yet; compare the three \
     readings with the monitor's OSD (Others > Firmware)";

/// One labelled part of the firmware group, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub code: u8,
    pub label: &'static str,
    /// `None` when the read failed or returned [`FIRMWARE_READ_FAILURE`].
    pub raw: Option<u16>,
    pub reading: String,
}

/// The `0xC8` / `0xFD` / `0xC9` readings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Firmware {
    pub controller: Option<u16>,
    pub firmware_byte: Option<u16>,
    pub firmware_level: Option<u16>,
}

impl Firmware {
    /// Build from `(code, value)` reads. A missing code or `0xFFFF` is `None`.
    pub fn from_reads(reads: &[(u8, u16)]) -> Firmware {
        let pick = |code: u8| {
            reads
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, v)| *v)
                .filter(|v| *v != FIRMWARE_READ_FAILURE)
        };
        Firmware {
            controller: pick(0xC8),
            firmware_byte: pick(0xFD),
            firmware_level: pick(0xC9),
        }
    }

    /// All three read successfully.
    pub fn is_complete(&self) -> bool {
        self.controller.is_some() && self.firmware_byte.is_some() && self.firmware_level.is_some()
    }

    /// Firmware version as the OSD shows it, e.g. "M2T101" from C8=0x5605
    /// FD=0x0074 C9=0x4101.
    ///
    /// ```text
    /// M2   <- 0xC8 low byte 0x05, the scaler prefix
    /// T    <- 0xFD low byte 0x74 = 't', uppercased
    /// 101  <- 0xC9 low 12 bits, three hex digits
    /// ```
    ///
    /// The letter and digits come straight from the bytes; the "M2" prefix for
    /// scaler 0x05 is fitted to one panel, so any other scaler returns `None`.
    pub fn version(&self) -> Option<String> {
        let prefix = match (self.controller? & 0xFF) as u8 {
            0x05 => "M2",
            _ => return None,
        };
        let letter = ((self.firmware_byte? & 0xFF) as u8 as char).to_ascii_uppercase();
        Some(format!(
            "{prefix}{letter}{:03X}",
            self.firmware_level? & 0x0FFF
        ))
    }

    /// The scaler vendor from `0xC8`'s low byte, or `None` if it didn't read
    /// or isn't one of the three DDPM knows. The byte is read from the panel,
    /// but the byte-to-vendor table is DDPM's and hasn't been checked, so the
    /// decode is inferred.
    pub fn controller_vendor(&self) -> Option<&'static str> {
        match (self.controller? & 0xFF) as u8 {
            0x05 => Some("mstar-mediatek"),
            0x09 => Some("realtek"),
            0x12 => Some("novatek"),
            _ => None,
        }
    }

    /// The three readings, labelled, in DDPM's read order.
    pub fn components(&self) -> Vec<Component> {
        let missing = || String::from("did not answer");
        vec![
            Component {
                code: 0xC8,
                label: "display-controller",
                raw: self.controller,
                reading: match self.controller {
                    Some(v) => {
                        format!(
                            "0x{v:04X}  (low byte 0x{:02X} selects the scaler vendor)",
                            v & 0xFF
                        )
                    }
                    None => missing(),
                },
            },
            Component {
                code: 0xFD,
                label: "firmware-byte",
                raw: self.firmware_byte,
                reading: match self.firmware_byte {
                    Some(v) => {
                        let c = (v & 0xFF) as u8;
                        let printable = if (0x20..=0x7E).contains(&c) {
                            format!("'{}'", c as char)
                        } else {
                            String::from("not printable")
                        };
                        format!("0x{v:04X}  ({printable}, the version letter)")
                    }
                    None => missing(),
                },
            },
            Component {
                code: 0xC9,
                label: "firmware-level",
                raw: self.firmware_level,
                reading: match self.firmware_level {
                    Some(v) => format!(
                        "0x{v:04X}  (low 12 bits 0x{:03X} are the version digits)",
                        v & 0x0FFF
                    ),
                    None => missing(),
                },
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_match_distinguishes_all_four_cases() {
        let a = MonitorKey::new("U4323QE", "ABC123");
        assert_eq!(
            match_keys(&a, &MonitorKey::new("u4323qe", "ABC123")),
            KeyMatch::Exact
        );
        assert_eq!(
            match_keys(&a, &MonitorKey::new("U4323QE", "ZZZ999")),
            KeyMatch::SameModel
        );
        assert_eq!(
            match_keys(&a, &MonitorKey::new("U4323QE", "")),
            KeyMatch::Unknown
        );
        assert_eq!(
            match_keys(&a, &MonitorKey::new("U2723QE", "ABC123")),
            KeyMatch::Different
        );
    }

    #[test]
    fn a_missing_serial_never_counts_as_a_match() {
        let blank = MonitorKey::new("U4323QE", "");
        assert_eq!(match_keys(&blank, &blank), KeyMatch::Unknown);
        assert!(!blank.is_complete());
        assert_eq!(blank.to_string(), "U4323QE/unknown-serial");
    }

    #[test]
    fn firmware_components_are_labelled_in_read_order() {
        let fw = Firmware::from_reads(&[(0xC8, 0x0005), (0xFD, 0x0074), (0xC9, 0x0001)]);
        assert!(fw.is_complete());
        let parts = fw.components();
        assert_eq!(
            parts.iter().map(|c| c.code).collect::<Vec<_>>(),
            vec![0xC8, 0xFD, 0xC9]
        );
        assert!(parts[1].reading.contains("'t'"), "{}", parts[1].reading);
        assert!(parts[2].reading.contains("0x001"), "{}", parts[2].reading);
    }

    #[test]
    fn ffff_is_a_failed_read_not_a_firmware_value() {
        let fw = Firmware::from_reads(&[(0xC8, 0xFFFF), (0xFD, 0x0074)]);
        assert!(!fw.is_complete());
        assert_eq!(fw.controller, None);
        assert_eq!(fw.controller_vendor(), None);
        assert_eq!(fw.components()[0].reading, "did not answer");
    }

    #[test]
    fn the_controller_low_byte_decodes_to_a_vendor() {
        let vendor = |v: u16| Firmware::from_reads(&[(0xC8, v)]).controller_vendor();
        assert_eq!(vendor(0x0005), Some("mstar-mediatek"));
        assert_eq!(
            vendor(0x1209),
            Some("realtek"),
            "only the low byte selects the vendor"
        );
        assert_eq!(vendor(0x0012), Some("novatek"));
        assert_eq!(vendor(0x0077), None);
    }

    #[test]
    fn version_reproduces_the_osd_string() {
        let fw = Firmware::from_reads(&[(0xC8, 0x5605), (0xFD, 0x0074), (0xC9, 0x4101)]);
        assert_eq!(fw.version().as_deref(), Some("M2T101"));
    }

    #[test]
    fn version_is_none_for_an_unseen_scaler() {
        let fw = Firmware::from_reads(&[(0xC8, 0x0009), (0xFD, 0x0074), (0xC9, 0x4101)]);
        assert_eq!(fw.version(), None);
    }

    #[test]
    fn version_is_none_when_a_read_failed() {
        let fw = Firmware::from_reads(&[(0xC8, 0x5605), (0xFD, 0xFFFF), (0xC9, 0x4101)]);
        assert_eq!(fw.version(), None);
    }
}
