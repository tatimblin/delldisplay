//! Parser for the MCCS capabilities string (VCP 0xF3).

/// One `vcp(...)` entry: a feature code plus its enumerated legal values, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcpEntry {
    pub code: u8,
    /// Legal values for non-continuous codes. Empty for continuous ones.
    pub values: Vec<u8>,
}

/// A parsed capabilities string. Missing sections come back as `None` or empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub prot: Option<String>,
    /// The `type(...)` section, e.g. `lcd`.
    pub kind: Option<String>,
    pub model: Option<String>,
    pub mccs_ver: Option<String>,
    pub cmds: Vec<u8>,
    pub vcp: Vec<VcpEntry>,
    pub raw: String,
}

/// Index just past the `)` that closes a group whose `(` sits before `from`.
fn close_paren(b: &[u8], from: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in b.iter().enumerate().skip(from) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The balanced-paren body of `key(...)`, if present.
fn section<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let b = s.as_bytes();
    let pat = format!("{key}(");
    let mut from = 0;
    while let Some(rel) = s[from..].find(&pat) {
        let start = from + rel;
        // skip matches that are the tail of a longer identifier
        let tail = start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_');
        if !tail {
            let open = start + pat.len();
            return close_paren(b, open).map(|end| &s[open..end]);
        }
        from = start + 1;
    }
    None
}

fn hex_list(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .filter_map(|t| u8::from_str_radix(t, 16).ok())
        .collect()
}

/// Parse a `vcp(...)` body into entries, honouring nested `CODE(v v v)` groups.
fn parse_vcp(body: &str) -> Vec<VcpEntry> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let code = u8::from_str_radix(&body[start..i], 16).ok();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        // Always consume a value group, even for a code we can't use, so its
        // values aren't read as top-level codes.
        let mut values = Vec::new();
        if i < b.len() && b[i] == b'(' {
            let end = close_paren(b, i + 1).unwrap_or(b.len());
            values = hex_list(&body[i + 1..end]);
            i = end + 1;
        }
        if let Some(code) = code {
            out.push(VcpEntry { code, values });
        }
    }
    out
}

impl Capabilities {
    /// Parse a raw capabilities string. Never fails; unknown sections are ignored.
    pub fn parse(raw: &str) -> Self {
        Capabilities {
            prot: section(raw, "prot").map(str::to_string),
            kind: section(raw, "type").map(str::to_string),
            model: section(raw, "model").map(str::to_string),
            mccs_ver: section(raw, "mccs_ver").map(str::to_string),
            cmds: section(raw, "cmds").map(hex_list).unwrap_or_default(),
            vcp: section(raw, "vcp").map(parse_vcp).unwrap_or_default(),
            raw: raw.to_string(),
        }
    }

    /// Whether the display advertises `code`.
    pub fn supports(&self, code: u8) -> bool {
        self.vcp.iter().any(|e| e.code == code)
    }

    /// Advertised values for `code`: `None` if not advertised, empty if continuous.
    pub fn legal_values(&self, code: u8) -> Option<&[u8]> {
        self.vcp
            .iter()
            .find(|e| e.code == code)
            .map(|e| e.values.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::U4323QE;

    #[test]
    fn parses_real_capabilities() {
        let c = Capabilities::parse(U4323QE);
        assert_eq!(c.model.as_deref(), Some("U4323QE"));
        assert_eq!(c.mccs_ver.as_deref(), Some("2.1"));
        assert_eq!(c.kind.as_deref(), Some("lcd"));
        assert_eq!(c.cmds, vec![0x01, 0x02, 0x03, 0x07, 0x0C, 0xE3, 0xF3]);
        assert!(c.supports(0x60));
        assert!(c.supports(0xE9));
        assert!(!c.supports(0x99));
        assert_eq!(
            c.legal_values(0x60),
            Some(&[0x1B, 0x0F, 0x13, 0x11, 0x12][..])
        );
        assert_eq!(c.legal_values(0xE7), Some(&[0x00, 0x01, 0x02, 0x03][..]));
        assert_eq!(c.legal_values(0xE9).unwrap().len(), 13);
        // continuous codes carry no value list
        assert_eq!(c.legal_values(0x10), Some(&[][..]));
    }

    #[test]
    fn an_oversized_code_takes_its_value_group_with_it() {
        let c = Capabilities::parse("vcp(10 1FF(02 04) 12)");
        let codes: Vec<u8> = c.vcp.iter().map(|e| e.code).collect();
        assert_eq!(codes, vec![0x10, 0x12]);
    }

    #[test]
    fn a_section_name_inside_a_longer_one_is_skipped() {
        let c = Capabilities::parse("(xtype(crt)type(lcd))");
        assert_eq!(c.kind.as_deref(), Some("lcd"));
    }
}
