//! EDID 1.x base-block parsing: which monitor this is.
//!
//! Read over I2C from the EEPROM at 0x50, not a VCP code. Decodes the VESA
//! base block only; extension blocks are counted, not read.

/// Length of the EDID base block.
pub const EDID_LEN: usize = 128;

/// The fixed header every EDID base block starts with.
pub const MAGIC: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// Offsets of the four 18-byte descriptor slots.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdidError {
    /// Fewer than 128 bytes were available.
    TooShort(usize),
    /// The 8-byte header was not `00 FF FF FF FF FF FF 00`. Almost always means
    /// the read landed on the wrong chip or returned zeros.
    BadHeader([u8; 8]),
}

impl std::fmt::Display for EdidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EdidError::TooShort(n) => {
                write!(f, "EDID too short: {n} bytes, need {EDID_LEN}")
            }
            EdidError::BadHeader(h) => {
                let hex: Vec<String> = h.iter().map(|b| format!("{b:02X}")).collect();
                write!(
                    f,
                    "not an EDID block: header is {} (expected 00 FF FF FF FF FF FF 00)",
                    hex.join(" ")
                )
            }
        }
    }
}
impl std::error::Error for EdidError {}

/// What a descriptor slot turned out to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Descriptor {
    /// Tag 0xFC: the model name, e.g. `U4323QE`.
    Name(String),
    /// Tag 0xFF: the serial-number string. Dell puts the real serial here.
    Serial(String),
    /// Tag 0xFE: unspecified text. Dell has been seen to put the panel
    /// identifier or a blank here; treat it as a label, not as identity.
    Text(String),
    /// Tag 0xFD: vertical/horizontal range limits and max pixel clock.
    RangeLimits {
        v_hz: (u8, u8),
        h_khz: (u8, u8),
        max_clock_mhz: Option<u16>,
    },
    /// A detailed timing descriptor (the first is the preferred mode).
    Timing(Timing),
    /// A display descriptor this module does not decode, kept so the slot is
    /// never silently dropped.
    Other { tag: u8 },
    /// An unused slot (tag 0x10 or an all-zero block).
    Unused,
}

/// A detailed timing descriptor, reduced to the parts that identify a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Pixel clock in kHz (the descriptor stores tens of kHz).
    pub pixel_clock_khz: u32,
    pub h_active: u16,
    pub v_active: u16,
    pub interlaced: bool,
}

impl std::fmt::Display for Timing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}{}",
            self.h_active,
            self.v_active,
            if self.interlaced { "i" } else { "" }
        )?;
        if self.pixel_clock_khz > 0 {
            write!(
                f,
                " @ {:.2} MHz pixel clock",
                self.pixel_clock_khz as f64 / 1000.0
            )?;
        }
        Ok(())
    }
}

/// A parsed EDID base block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edid {
    /// Three-letter PNP manufacturer id, e.g. `DEL`.
    pub manufacturer: String,
    /// Manufacturer-assigned product code (little-endian in the block).
    pub product_code: u16,
    /// Binary serial number. `0` means unused; many panels put
    /// the real serial in a 0xFF descriptor instead.
    pub serial_number: u32,
    /// Week of manufacture, 1-54. `None` when unspecified or when the byte is
    /// 0xFF, which flags [`year`](Edid::year) as a model year.
    pub week: Option<u8>,
    /// Year of manufacture, or model year if [`year_is_model`](Edid::year_is_model).
    pub year: Option<u16>,
    /// True when byte 16 was 0xFF, meaning the year is a *model* year and there
    /// is no manufacture week at all.
    pub year_is_model: bool,
    /// EDID structure version, e.g. `(1, 4)`.
    pub version: (u8, u8),
    /// Number of extension blocks that follow this one. We only read the base
    /// block, so this is a count of what we did *not* see.
    pub extensions: u8,
    /// Whether the 128-byte block sums to zero mod 256.
    pub checksum_ok: bool,
    /// The four descriptor slots, in block order.
    pub descriptors: Vec<Descriptor>,
    /// The raw block, kept so a caller can dump or re-checksum it.
    pub raw: [u8; EDID_LEN],
}

/// Parse an EDID base block. Accepts any slice of at least 128 bytes.
///
/// A bad checksum is *not* an error: the identity fields are usually still
/// readable and refusing to name the monitor helps nobody. It is reported in
/// [`Edid::checksum_ok`] so a caller can flag the identity as suspect.
pub fn parse(bytes: &[u8]) -> Result<Edid, EdidError> {
    if bytes.len() < EDID_LEN {
        return Err(EdidError::TooShort(bytes.len()));
    }
    let mut raw = [0u8; EDID_LEN];
    raw.copy_from_slice(&bytes[..EDID_LEN]);

    if raw[..8] != MAGIC {
        let mut h = [0u8; 8];
        h.copy_from_slice(&raw[..8]);
        return Err(EdidError::BadHeader(h));
    }

    let manufacturer = decode_pnp(u16::from_be_bytes([raw[8], raw[9]]));
    let product_code = u16::from_le_bytes([raw[10], raw[11]]);
    let serial_number = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);

    let week_byte = raw[16];
    let year_byte = raw[17];
    let year_is_model = week_byte == 0xFF;
    let week = match week_byte {
        0x00 | 0xFF => None,
        w => Some(w),
    };
    let year = if year_byte == 0 {
        None
    } else {
        Some(1990 + year_byte as u16)
    };

    let descriptors = DESCRIPTORS
        .iter()
        .map(|o| descriptor(&raw[*o..*o + 18]))
        .collect();

    let checksum_ok = raw.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0;

    Ok(Edid {
        manufacturer,
        product_code,
        serial_number,
        week,
        year,
        year_is_model,
        version: (raw[18], raw[19]),
        extensions: raw[126],
        checksum_ok,
        descriptors,
        raw,
    })
}

impl Edid {
    /// The model-name string from a 0xFC descriptor, if the panel supplied one.
    pub fn model_name(&self) -> Option<&str> {
        self.descriptors.iter().find_map(|d| match d {
            Descriptor::Name(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
    }

    /// The serial string from a 0xFF descriptor, if the panel supplied one.
    pub fn serial_text(&self) -> Option<&str> {
        self.descriptors.iter().find_map(|d| match d {
            Descriptor::Serial(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
    }

    /// Every unspecified-text (0xFE) descriptor, in block order.
    pub fn texts(&self) -> Vec<&str> {
        self.descriptors
            .iter()
            .filter_map(|d| match d {
                Descriptor::Text(s) if !s.is_empty() => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The preferred timing: the first detailed timing descriptor.
    pub fn preferred_timing(&self) -> Option<Timing> {
        self.descriptors.iter().find_map(|d| match d {
            Descriptor::Timing(t) => Some(*t),
            _ => None,
        })
    }

    /// The best serial we have: the 0xFF string, else the binary serial in
    /// decimal, else `None`. `None` means this panel can't be told apart from
    /// another of the same model, so callers that key state on identity
    /// must treat that as a refusal, not as a blank key that matches anything.
    pub fn serial(&self) -> Option<String> {
        if let Some(s) = self.serial_text() {
            return Some(s.to_string());
        }
        if self.serial_number != 0 {
            return Some(self.serial_number.to_string());
        }
        None
    }

    /// The best model name we have: the 0xFC string, else the product code as
    /// `0xNNNN`. The product code always exists.
    pub fn model(&self) -> String {
        match self.model_name() {
            Some(m) => m.to_string(),
            None => format!("0x{:04X}", self.product_code),
        }
    }

    /// Expanded vendor name for the PNP id, for the handful this project can
    /// actually vouch for. Returns `None` rather than guessing.
    pub fn vendor_name(&self) -> Option<&'static str> {
        match self.manufacturer.as_str() {
            "DEL" => Some("Dell"),
            _ => None,
        }
    }

    /// One-line human name, e.g. `Dell U4323QE`.
    pub fn display_name(&self) -> String {
        let vendor = self.vendor_name().unwrap_or(&self.manufacturer);
        let model = self.model();
        if model
            .to_ascii_uppercase()
            .starts_with(&vendor.to_ascii_uppercase())
        {
            model
        } else {
            format!("{vendor} {model}")
        }
    }

    /// The date fields as text, in whichever of the three forms the panel used.
    pub fn manufacture_label(&self) -> String {
        match (self.year_is_model, self.week, self.year) {
            (true, _, Some(y)) => format!("model year {y}"),
            (false, Some(w), Some(y)) => format!("week {w} of {y}"),
            (false, None, Some(y)) => format!("{y} (week unspecified)"),
            _ => String::from("unspecified"),
        }
    }
}

/// Decode the packed 5-bit-per-letter PNP manufacturer id.
fn decode_pnp(v: u16) -> String {
    let letter = |shift: u16| -> char {
        let n = ((v >> shift) & 0x1F) as u8;
        if (1..=26).contains(&n) {
            (b'A' + n - 1) as char
        } else {
            '?'
        }
    };
    [letter(10), letter(5), letter(0)].iter().collect()
}

/// Decode one 18-byte descriptor slot.
fn descriptor(d: &[u8]) -> Descriptor {
    if d.iter().all(|b| *b == 0) {
        return Descriptor::Unused;
    }
    // A display descriptor has a zero pixel clock (bytes 0-1) and a zero
    // reserved byte 2. Anything else is a detailed timing.
    if d[0] == 0 && d[1] == 0 && d[2] == 0 {
        return match d[3] {
            0xFF => Descriptor::Serial(text(&d[5..18])),
            0xFE => Descriptor::Text(text(&d[5..18])),
            0xFC => Descriptor::Name(text(&d[5..18])),
            0xFD => Descriptor::RangeLimits {
                v_hz: (d[5], d[6]),
                h_khz: (d[7], d[8]),
                // Byte 9 is max pixel clock in units of 10 MHz; 0 = unspecified.
                max_clock_mhz: if d[9] == 0 {
                    None
                } else {
                    Some(d[9] as u16 * 10)
                },
            },
            0x10 => Descriptor::Unused,
            tag => Descriptor::Other { tag },
        };
    }
    Descriptor::Timing(Timing {
        pixel_clock_khz: u16::from_le_bytes([d[0], d[1]]) as u32 * 10,
        h_active: d[2] as u16 | ((d[4] as u16 & 0xF0) << 4),
        v_active: d[5] as u16 | ((d[7] as u16 & 0xF0) << 4),
        interlaced: d[17] & 0x80 != 0,
    })
}

/// Descriptor strings end at 0x0A and are space-padded. Non-printable bytes are
/// dropped, since a control byte in a monitor name is corruption.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0x0A).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .filter(|b| (0x20..=0x7E).contains(*b))
        .map(|b| *b as char)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a base block from the VESA spec (not captured from hardware).
    fn build(name: &str, serial: &str, week: u8, year_byte: u8) -> [u8; EDID_LEN] {
        let mut e = [0u8; EDID_LEN];
        e[..8].copy_from_slice(&MAGIC);
        // "DEL" packed 5 bits per letter: D=4, E=5, L=12.
        let id: u16 = (4 << 10) | (5 << 5) | 12;
        e[8..10].copy_from_slice(&id.to_be_bytes());
        e[10..12].copy_from_slice(&0xA0D2u16.to_le_bytes());
        e[12..16].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        e[16] = week;
        e[17] = year_byte;
        e[18] = 1;
        e[19] = 4;

        // Descriptor 0: preferred timing, 3840x2160, 533.25 MHz.
        let d = &mut e[54..72];
        d[0..2].copy_from_slice(&53325u16.to_le_bytes());
        d[2] = (3840 & 0xFF) as u8;
        d[4] = ((3840u16 >> 8) << 4) as u8;
        d[5] = (2160 & 0xFF) as u8;
        d[7] = ((2160u16 >> 8) << 4) as u8;

        let mut put = |off: usize, tag: u8, s: &str| {
            let d = &mut e[off..off + 18];
            d[0] = 0;
            d[1] = 0;
            d[2] = 0;
            d[3] = tag;
            d[4] = 0;
            for (i, b) in s.bytes().take(13).enumerate() {
                d[5 + i] = b;
            }
            if s.len() < 13 {
                d[5 + s.len()] = 0x0A;
                for i in s.len() + 1..13 {
                    d[5 + i] = b' ';
                }
            }
        };
        put(72, 0xFF, serial);
        put(90, 0xFC, name);
        put(108, 0x10, "");

        e[126] = 1; // one extension block we never read
        fix_checksum(&mut e);
        e
    }

    /// Set byte 127 so the block sums to zero.
    fn fix_checksum(e: &mut [u8; EDID_LEN]) {
        let sum = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        e[127] = sum.wrapping_neg();
    }

    fn sample() -> Edid {
        parse(&build("U4323QE", "ABC123XYZ", 23, 33)).unwrap()
    }

    #[test]
    fn rejects_a_block_that_is_not_an_edid() {
        assert_eq!(parse(&[0u8; 128]), Err(EdidError::BadHeader([0; 8])));
        assert_eq!(parse(&[0u8; 12]), Err(EdidError::TooShort(12)));
    }

    #[test]
    fn decodes_the_packed_manufacturer_id() {
        assert_eq!(sample().manufacturer, "DEL");
        assert_eq!(sample().vendor_name(), Some("Dell"));
    }

    #[test]
    fn reads_model_serial_and_date() {
        let e = sample();
        assert_eq!(e.model_name(), Some("U4323QE"));
        assert_eq!(e.serial_text(), Some("ABC123XYZ"));
        assert_eq!(e.manufacture_label(), "week 23 of 2023");
        assert_eq!(e.version, (1, 4));
        assert_eq!(e.product_code, 0xA0D2);
        assert_eq!(e.serial_number, 0x0102_0304);
        assert!(e.checksum_ok, "the builder must produce a valid checksum");
        assert_eq!(e.extensions, 1);
    }

    #[test]
    fn descriptor_string_stops_at_the_terminator() {
        // Padding spaces and the 0x0A terminator must not reach the caller.
        let e = parse(&build("U43", "S1", 1, 30)).unwrap();
        assert_eq!(e.model_name(), Some("U43"));
        assert_eq!(e.serial_text(), Some("S1"));
    }

    #[test]
    fn model_year_form_is_not_reported_as_a_manufacture_date() {
        // Byte 16 == 0xFF: the year is a model year and there's no week.
        let e = parse(&build("U4323QE", "S", 0xFF, 34)).unwrap();
        assert!(e.year_is_model);
        assert_eq!(e.week, None);
        assert_eq!(e.manufacture_label(), "model year 2024");
    }

    #[test]
    fn preferred_timing_comes_out_of_the_first_detailed_descriptor() {
        let t = sample().preferred_timing().unwrap();
        assert_eq!((t.h_active, t.v_active), (3840, 2160));
        assert_eq!(t.pixel_clock_khz, 533_250);
        assert!(!t.interlaced);
    }

    #[test]
    fn an_all_zero_slot_is_unused() {
        assert_eq!(descriptor(&[0; 18]), Descriptor::Unused);
        let e = sample();
        assert_eq!(e.descriptors[3], Descriptor::Unused); // tag 0x10
    }

    #[test]
    fn fix_checksum_makes_the_block_sum_to_zero() {
        let mut e = build("U4323QE", "S", 5, 33);
        e[20] ^= 0x5A;
        fix_checksum(&mut e);
        assert_eq!(e.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    }

    #[test]
    fn a_bad_checksum_is_reported_not_fatal() {
        let mut bytes = build("U4323QE", "ABC123XYZ", 23, 33);
        bytes[127] ^= 0xFF;
        let e = parse(&bytes).unwrap();
        assert!(!e.checksum_ok);
        // Identity is still readable, which is why it isn't an error.
        assert_eq!(e.model_name(), Some("U4323QE"));
    }

    #[test]
    fn a_panel_with_no_serial_at_all_has_none() {
        let mut bytes = build("U4323QE", "", 5, 33);
        bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
        fix_checksum(&mut bytes);
        let e = parse(&bytes).unwrap();
        assert_eq!(e.serial(), None);
        assert_eq!(e.display_name(), "Dell U4323QE");
    }

    #[test]
    fn binary_serial_is_the_fallback_when_there_is_no_serial_string() {
        let mut bytes = build("U4323QE", "", 5, 33);
        fix_checksum(&mut bytes);
        let e = parse(&bytes).unwrap();
        assert_eq!(e.serial(), Some("16909060".to_string()));
    }

    #[test]
    fn unknown_vendor_is_not_invented() {
        let mut bytes = build("X1", "S", 5, 33);
        let id: u16 = (1 << 10) | (2 << 5) | 3; // "ABC"
        bytes[8..10].copy_from_slice(&id.to_be_bytes());
        fix_checksum(&mut bytes);
        let e = parse(&bytes).unwrap();
        assert_eq!(e.manufacturer, "ABC");
        assert_eq!(e.vendor_name(), None);
        assert_eq!(e.display_name(), "ABC X1");
    }
}
