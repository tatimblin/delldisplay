//! Per-panel VCP facts and quirks, compiled from `profiles/*.toml`.
//! Codes a profile doesn't list fall back to the MCCS baseline in `mccs.toml`.
#![forbid(unsafe_code)]

/// How much a claim is worth trusting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Confirmed by reading or writing this panel.
    Observed,
    /// Derived from DDPM's code or a capture, but not exercised here.
    Inferred,
    /// MCCS standard, untested on this panel.
    Spec,
    /// Advertised by the panel; meaning not established.
    Unknown,
}

impl Provenance {
    pub fn label(self) -> &'static str {
        match self {
            Provenance::Observed => "observed",
            Provenance::Inferred => "inferred",
            Provenance::Spec => "spec",
            Provenance::Unknown => "unknown",
        }
    }
    /// Only `Observed` counts as verified against hardware.
    pub fn is_verified(self) -> bool {
        matches!(self, Provenance::Observed)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodeInfo {
    pub vcp: u8,
    pub name: &'static str,
    pub provenance: Provenance,
    pub note: &'static str,
    /// Enumerated values, if the code has them.
    pub values: &'static [(u8, &'static str)],
}

impl CodeInfo {
    /// Human name for one of this code's values.
    pub fn value_name(&self, value: u8) -> Option<&'static str> {
        self.values
            .iter()
            .find(|(k, _)| *k == value)
            .map(|(_, v)| *v)
    }

    /// Value for a symbolic name, e.g. "usb-c" -> 0x1B on 0x60.
    pub fn value_by_name(&self, name: &str) -> Option<u8> {
        let n = name.trim().to_ascii_lowercase();
        self.values.iter().find(|(_, v)| *v == n).map(|(k, _)| *k)
    }
}

/// Panel behaviours that deviate from the MCCS spec.
#[derive(Debug, Clone, Copy)]
pub struct Quirks {
    /// Every request frame must be written twice.
    pub double_write: bool,
    /// Whether the reply's type byte can be trusted.
    pub type_byte_reliable: bool,
    /// Result byte used to decline a feature over DDC.
    pub refusal_result: u8,
    /// Codes whose value is an enum in the low byte regardless of the type byte.
    pub enumerated: &'static [u8],
    /// Codes needing the longer settle delay around a write.
    pub slow_write_codes: &'static [u8],
    pub slow_write_ms: u64,
    pub normal_write_ms: u64,
}

/// One compiled panel profile.
#[derive(Debug, Clone, Copy)]
pub struct Panel {
    /// The first of `models`.
    pub model: &'static str,
    /// Every EDID model name this profile covers.
    pub models: &'static [&'static str],
    pub mccs: &'static str,
    /// Only the codes this profile lists; lookups also consult [`baseline`].
    pub codes: &'static [CodeInfo],
    pub quirks: Quirks,
}

include!(concat!(env!("OUT_DIR"), "/panel_data.rs"));

/// Profiles are compared by model; each one's is unique.
impl PartialEq for Panel {
    fn eq(&self, other: &Self) -> bool {
        self.model == other.model
    }
}
impl Eq for Panel {}

impl Panel {
    /// Whether this profile covers `model`, ignoring case and surrounding space.
    pub fn matches(&self, model: &str) -> bool {
        let want = model.trim();
        self.models.iter().any(|m| m.eq_ignore_ascii_case(want))
    }

    /// This profile's entry for `vcp`, else the MCCS baseline's.
    pub fn lookup(&self, vcp: u8) -> Option<&'static CodeInfo> {
        let codes: &'static [CodeInfo] = self.codes;
        codes.iter().chain(BASELINE_CODES).find(|c| c.vcp == vcp)
    }

    pub fn by_name(&self, name: &str) -> Option<&'static CodeInfo> {
        let n = name.trim().to_ascii_lowercase();
        let codes: &'static [CodeInfo] = self.codes;
        codes.iter().chain(BASELINE_CODES).find(|c| c.name == n)
    }

    pub fn value_name(&self, vcp: u8, value: u8) -> Option<&'static str> {
        self.lookup(vcp)?.value_name(value)
    }

    pub fn value_by_name(&self, vcp: u8, name: &str) -> Option<u8> {
        self.lookup(vcp)?.value_by_name(name)
    }

    pub fn is_enumerated(&self, vcp: u8) -> bool {
        self.quirks.enumerated.contains(&vcp)
    }

    pub fn post_write_delay_ms(&self, vcp: u8) -> u64 {
        let q = &self.quirks;
        if q.slow_write_codes.contains(&vcp) {
            q.slow_write_ms
        } else {
            q.normal_write_ms
        }
    }
}

/// The standard MCCS codes every profile falls back to.
pub fn baseline() -> &'static [CodeInfo] {
    BASELINE_CODES
}

/// The profile covering `model` (e.g. from EDID), if any.
pub fn for_model(model: &str) -> Option<&'static Panel> {
    PROFILES.iter().find(|p| p.matches(model))
}

/// The U4323QE profile, used when a panel's model isn't known.
pub fn default_panel() -> &'static Panel {
    &PROFILES[DEFAULT_INDEX]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_loads_and_is_self_consistent() {
        let p = default_panel();
        assert!(p.codes.len() >= 35, "expected the full advertised set");
        assert_eq!(p.model, "DELL U4323QE");
        for p in PROFILES.iter() {
            for w in p.codes.windows(2) {
                assert!(w[0].vcp < w[1].vcp, "{}: duplicate or unsorted", p.model);
            }
        }
    }

    #[test]
    fn observed_facts_survive_codegen() {
        let p = default_panel();
        assert_eq!(p.value_name(0x60, 0x1B), Some("usb-c"));
        assert_eq!(p.value_name(0x60, 0x13), Some("dp2"));
        assert_eq!(p.value_by_name(0xE9, "off"), Some(0x00));
        assert_eq!(p.value_name(0xE9, 0x24), Some("pbp-2up-self-left"));
        assert!(p.lookup(0xE8).unwrap().provenance.is_verified());
        assert!(!p.lookup(0xAC).unwrap().provenance.is_verified());
        assert!(!p.lookup(0xE2).unwrap().provenance.is_verified());
    }

    #[test]
    fn every_model_is_compiled_and_found() {
        let n: usize = PROFILES.iter().map(|p| p.models.len()).sum();
        assert_eq!(n, MODEL_COUNT);
        for p in PROFILES.iter() {
            assert_eq!(p.model, p.models[0]);
            for m in p.models {
                assert_eq!(for_model(m).unwrap().model, p.model);
            }
        }
    }

    #[test]
    fn model_lookup_ignores_case_and_space() {
        assert_eq!(for_model("dell u2723qx ").unwrap().model, "DELL U2723QE");
        assert!(for_model("DELL NOSUCH").is_none());
        assert_eq!(default_panel().model, "DELL U4323QE");
    }

    #[test]
    fn thin_profiles_fall_back_to_the_baseline() {
        let p = for_model("DELL U2722DE").unwrap();
        assert_eq!(p.by_name("brightness").unwrap().vcp, 0x10);
        assert_eq!(p.lookup(0x12).unwrap().provenance, Provenance::Spec);
        assert_eq!(p.value_by_name(0x60, "hdmi1"), Some(0x11));
        // The profile's own entry wins over the baseline.
        assert_eq!(p.lookup(0xC8).unwrap().provenance, Provenance::Inferred);
        assert_eq!(p.by_name("preset-mode").unwrap().vcp, 0xE2);
        assert_eq!(p.by_name("color-preset").unwrap().vcp, 0x14);
    }

    #[test]
    fn a_relabelled_code_takes_its_values_from_the_profile() {
        // 0x8D is the Speaker switch here: the MCCS name still finds the code,
        // but `muted` isn't one of its values, so it can't be written by name.
        let p = default_panel();
        assert_eq!(p.by_name("speaker").unwrap().vcp, 0x8D);
        assert_eq!(p.by_name("audio-mute").unwrap().vcp, 0x8D);
        assert_eq!(p.value_by_name(0x8D, "muted"), None);
        assert_eq!(p.value_by_name(0x8D, "speaker-off"), Some(0x00));
        // A thin profile keeps MCCS polarity.
        let thin = for_model("DELL U2722DE").unwrap();
        assert_eq!(thin.value_by_name(0x8D, "muted"), Some(0x01));
    }

    #[test]
    fn quirks_match_what_the_hardware_showed() {
        let p = default_panel();
        assert!(p.quirks.double_write);
        assert!(!p.quirks.type_byte_reliable);
        assert_eq!(p.quirks.refusal_result, 0x01);
        assert!(p.is_enumerated(0x60) && !p.is_enumerated(0xE7));
        assert_eq!(p.post_write_delay_ms(0xD6), 150);
        assert_eq!(p.post_write_delay_ms(0x10), 60);
    }
}
