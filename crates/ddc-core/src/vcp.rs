//! VCP code lookups, names and value resolution.
//!
//! Per-code names, values and quirks come from the `ddc-panels` profiles. The
//! U4323QE input and layout tables below (and [`crate::bits::panes`]) are
//! hardcoded.

pub use ddc_panels::{default_panel, for_model, CodeInfo, Panel, Provenance};

/// Named codes this project uses.
pub struct Vcp;
impl Vcp {
    /// MCCS: set once a control changes at the OSD; write 0x01 to clear.
    pub const NEW_CONTROL_VALUE: u8 = 0x02;
    /// MCCS: restore everything, OSD language included. Destructive.
    pub const RESTORE_FACTORY: u8 = 0x04;
    /// MCCS: restore brightness and contrast only.
    pub const RESTORE_LEVELS: u8 = 0x05;
    /// MCCS: restore colour only.
    pub const RESTORE_COLOR: u8 = 0x08;
    pub const BRIGHTNESS: u8 = 0x10;
    pub const CONTRAST: u8 = 0x12;
    pub const COLOR_PRESET: u8 = 0x14;
    /// MCCS: video gain red / green / blue.
    pub const GAIN_RED: u8 = 0x16;
    pub const GAIN_GREEN: u8 = 0x18;
    pub const GAIN_BLUE: u8 = 0x1A;
    /// MCCS: which code changed most recently. Self-clearing.
    pub const ACTIVE_CONTROL: u8 = 0x52;
    pub const INPUT_SOURCE: u8 = 0x60;
    pub const VOLUME: u8 = 0x62;
    /// MCCS: audio mute.
    pub const AUDIO_MUTE: u8 = 0x8D;
    pub const OSD_LOCK: u8 = 0xCA;
    pub const POWER_MODE: u8 = 0xD6;
    /// MCCS: picture mode.
    pub const PICTURE_MODE: u8 = 0xDC;
    pub const MCCS_VERSION: u8 = 0xDF;
    /// Dell: PowerNap dim (or the whole PowerNap bitfield on some panels).
    pub const POWERNAP_DIM: u8 = 0xE0;
    /// Dell: PowerNap sleep, on two-boolean panels.
    pub const POWERNAP_SLEEP: u8 = 0xE1;
    /// Dell: PiP/PBP layout. See [`pip`].
    pub const PIP_MODE: u8 = 0xE9;
    /// Dell: PiP/PBP sub-window sources, three 5-bit fields. See [`pip_sub`].
    pub const PIP_SUB_SOURCE: u8 = 0xE8;
    /// Dell: USB upstream / KVM. See [`crate::kvm`]; it has two write modes.
    pub const USB_KVM: u8 = 0xE7;
    /// Dell: PxP action register (write-only). 0xF010 swaps main and sub1.
    pub const PXP_ACTION: u8 = 0xE5;
    /// Dell: USB upstream port inventory, four nibbles. Read-only.
    pub const PORT_INVENTORY: u8 = 0xEE;
    /// Dell: feature bitmask. Bit 5 gates zoom/underscan in Dell's software.
    pub const FEATURES: u8 = 0xF1;
    /// Dell: status word. Bit 7 set means busy (OSD open or mid-operation).
    pub const STATUS: u8 = 0xF2;
}

/// U4323QE input codes for VCP 0x60.
pub mod input {
    pub const DISPLAY_PORT: u8 = 0x0F;
    pub const HDMI_1: u8 = 0x11;
    pub const HDMI_2: u8 = 0x12;
    pub const DP2: u8 = 0x13;
    /// USB-C / Thunderbolt upstream.
    pub const USB_C: u8 = 0x1B;
}

/// An input's short name, or its hex code when the profile doesn't name it.
pub fn input_name(panel: &Panel, code: u8) -> String {
    match panel.value_name(Vcp::INPUT_SOURCE, code) {
        Some(n) => n.to_string(),
        None => format!("0x{code:02X}"),
    }
}

/// An input named and numbered, e.g. `usb-c (0x1B)`. Used in every refusal and warning.
pub fn input_label(panel: &Panel, code: u8) -> String {
    match panel.value_name(Vcp::INPUT_SOURCE, code) {
        Some(n) => format!("{n} (0x{code:02X})"),
        None => format!("0x{code:02X}"),
    }
}

/// PiP/PBP layouts for VCP 0xE9, mapped on a U4323QE.
///
/// Names say where this host's picture lands. Sub windows are numbered
/// clockwise from main. The panel accepts 13 values but only 11 are distinct:
/// 0x01 and 0x02 mean different things depending on the current layout, see
/// [`write_effect`](pip::write_effect).
pub mod pip {
    pub const OFF: u8 = 0x00;

    /// PiP, this host as main, small inset.
    pub const PIP_SMALL: u8 = 0x21;
    /// PiP, this host as main, larger inset.
    pub const PIP_LARGE: u8 = 0x22;

    /// 2 panes side by side: this host left, sub1 right.
    pub const PBP_2UP_SELF_LEFT: u8 = 0x24;
    /// 3 panes: this host full-height right, sub2 top-left, sub1 bottom-left.
    pub const PBP_L2R1: u8 = 0x32;
    /// 3 panes: this host full-height left, sub1 top-right, sub2 bottom-right.
    pub const PBP_3UP_LEFT_PLUS_STACK: u8 = 0x31;

    /// 2 panes stacked: this host top, sub1 bottom.
    pub const PBP_2UP_VERTICAL_SELF_TOP: u8 = 0x2F;
    /// 3 panes: this host full-width top, sub1 bottom-right, sub2 bottom-left.
    pub const PBP_VSTACK_SELF_TOP: u8 = 0x33;
    /// 3 panes: this host full-width bottom, sub1 top-left, sub2 top-right.
    pub const PBP_T2B1: u8 = 0x35;

    /// 3 columns, this host leftmost.
    pub const SPLIT_3UP_SELF_LEFT: u8 = 0x34;
    /// 2x2: main top-left, sub1 top-right, sub2 bottom-right, sub3 bottom-left.
    pub const QUAD_SELF_LEFT_COLUMN: u8 = 0x41;

    /// Write-aliases: layouts from off, step actions from PiP.
    pub const ALIAS_LR_SELF_RIGHT: u8 = 0x01;
    pub const ALIAS_LR_SELF_LEFT: u8 = 0x02;

    /// Every value the panel accepts.
    pub const ALL_LEGAL: [u8; 13] = [
        0x00, 0x01, 0x02, 0x21, 0x22, 0x24, 0x2F, 0x31, 0x32, 0x33, 0x34, 0x35, 0x41,
    ];

    /// The 11 distinct layouts, aliases removed.
    pub const CANONICAL: [u8; 11] = [
        0x00, 0x21, 0x22, 0x24, 0x2F, 0x31, 0x32, 0x33, 0x34, 0x35, 0x41,
    ];

    /// What writing a byte to 0xE9 does.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Effect {
        /// Selects this layout.
        Layout(u8),
        /// Moves the PiP inset to the next corner. Only from a PiP layout.
        StepInsetCorner,
        /// Toggles the PiP inset size, 0x21 <-> 0x22. Only from a PiP layout.
        StepInsetSize,
    }

    /// What writing `value` does while the panel shows `current` (a canonical layout).
    ///
    /// From off, 0x01 and 0x02 read back as 0x32 and 0x24. From PiP the same
    /// bytes step the inset: 0x01 changes its size, 0x02 moves its corner.
    pub fn write_effect(current: u8, value: u8) -> Effect {
        let in_pip = matches!(current, PIP_SMALL | PIP_LARGE);
        match value {
            ALIAS_LR_SELF_RIGHT if in_pip => Effect::StepInsetSize,
            ALIAS_LR_SELF_LEFT if in_pip => Effect::StepInsetCorner,
            ALIAS_LR_SELF_RIGHT => Effect::Layout(PBP_L2R1),
            ALIAS_LR_SELF_LEFT => Effect::Layout(PBP_2UP_SELF_LEFT),
            v => Effect::Layout(v),
        }
    }

    /// Fold a write-alias as it behaves from off.
    ///
    /// Only right when the panel is off, or for a value read back from 0xE9
    /// (always canonical already). Use [`write_effect`] when the current layout is known.
    pub fn canonical(value: u8) -> u8 {
        match write_effect(OFF, value) {
            Effect::Layout(l) => l,
            _ => value,
        }
    }
}

/// PiP/PBP sub-window sources: VCP 0xE8 packs three 5-bit fields.
///
/// ```text
/// E8 = sub1 | (sub2 << 5) | (sub3 << 10)      bit 15 unused
/// ```
///
/// Each field holds a 0x60 input code, 0 means unassigned, and main's source
/// stays on 0x60. The model predicted 0x6DEF (sub1 = dp1) before it was ever
/// written, and the panel accepted it; 0x6DF0 is refused because input 0x10
/// isn't one of this panel's inputs.
pub mod pip_sub {
    /// Decode the three sub-window source codes from a 0xE8 reading.
    pub fn decode(word: u16) -> [u8; 3] {
        [
            (word & 0x1F) as u8,
            ((word >> 5) & 0x1F) as u8,
            ((word >> 10) & 0x1F) as u8,
        ]
    }

    /// Pack three sub-window source codes into a 0xE8 value.
    pub fn encode(subs: [u8; 3]) -> u16 {
        (subs[0] as u16 & 0x1F) | ((subs[1] as u16 & 0x1F) << 5) | ((subs[2] as u16 & 0x1F) << 10)
    }

    /// Replace sub1's source, keeping sub2 and sub3. A 2-pane layout only has sub1.
    pub fn with_sub1(current: u16, input_code: u8) -> u16 {
        (current & !0x1F) | (input_code as u16 & 0x1F)
    }

    /// Sub1's input code.
    pub fn sub1_of(current: u16) -> u8 {
        (current & 0x1F) as u8
    }
}

/// Resolve a code name (`brightness`) or hex literal (`0xE9`, `e9`).
pub fn resolve(panel: &Panel, name: &str) -> Option<u8> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(hex) = n.strip_prefix("0x") {
        return u8::from_str_radix(hex, 16).ok();
    }
    if let Some(c) = panel.by_name(&n) {
        return Some(c.vcp);
    }
    if n.len() == 2 && n.chars().all(|c| c.is_ascii_hexdigit()) {
        return u8::from_str_radix(&n, 16).ok();
    }
    None
}

/// Resolve a value for `code`: hex (`0x1B`), profile name (`usb-c`), a short
/// alias (`dp`, `quad`), or decimal.
pub fn resolve_value(panel: &Panel, code: u8, value: &str) -> Option<u16> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix("0x") {
        return u16::from_str_radix(hex, 16).ok();
    }
    if let Some(byname) = panel.value_by_name(code, &v) {
        return Some(byname.into());
    }
    // Shorthands the profile doesn't spell.
    let alias = match (code, v.as_str()) {
        (0x60, "dp" | "displayport") => Some(input::DISPLAY_PORT),
        (0x60, "hdmi") => Some(input::HDMI_1),
        (0x60, "usbc") => Some(input::USB_C),
        (0xE9, "pip" | "on") => Some(pip::PIP_SMALL),
        (0xE9, "2up-left") => Some(pip::PBP_2UP_SELF_LEFT),
        (0xE9, "3up-stack") => Some(pip::PBP_3UP_LEFT_PLUS_STACK),
        (0xE9, "vsplit-top") => Some(pip::PBP_2UP_VERTICAL_SELF_TOP),
        (0xE9, "vstack-top") => Some(pip::PBP_VSTACK_SELF_TOP),
        (0xE9, "vsplit-bottom") => Some(pip::PBP_T2B1),
        (0xE9, "3up") => Some(pip::SPLIT_3UP_SELF_LEFT),
        (0xE9, "quad") => Some(pip::QUAD_SELF_LEFT_COLUMN),
        _ => None,
    };
    alias.map(u16::from).or_else(|| v.parse().ok())
}

/// Are two readings of `code` the same value?
///
/// Dell repeats enumerated values in the high byte (0x60 reads 0x1B1B for
/// USB-C, and its high byte also carries the arriving port), and 0x62/0x8D keep
/// OSD status bits up there, so those codes compare on the low byte only.
pub fn same_reading(panel: &Panel, code: u8, a: u16, b: u16) -> bool {
    let low_byte_only = panel.is_enumerated(code) || matches!(code, Vcp::VOLUME | Vcp::AUDIO_MUTE);
    a == b || (low_byte_only && a & 0xFF == b & 0xFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(name: &str) -> Option<u8> {
        super::resolve(default_panel(), name)
    }

    fn resolve_value(code: u8, value: &str) -> Option<u16> {
        super::resolve_value(default_panel(), code, value)
    }

    fn same_reading(code: u8, a: u16, b: u16) -> bool {
        super::same_reading(default_panel(), code, a, b)
    }

    fn describe_value(code: u8, value: u8) -> Option<&'static str> {
        default_panel().value_name(code, value)
    }

    #[test]
    fn resolves_names_and_hex() {
        assert_eq!(resolve("brightness"), Some(0x10));
        assert_eq!(resolve("pip"), Some(0xE9));
        assert_eq!(resolve("0xE9"), Some(0xE9));
        assert_eq!(resolve("e9"), Some(0xE9));
        assert_eq!(resolve("nope"), None);
    }

    #[test]
    fn same_reading_ignores_the_high_byte_only_where_dell_reuses_it() {
        assert!(same_reading(Vcp::INPUT_SOURCE, 0x1B1B, 0x001B));
        assert!(same_reading(Vcp::VOLUME, 0x8032, 0x0032));
        assert!(!same_reading(Vcp::BRIGHTNESS, 0x0132, 0x0032));
        assert!(!same_reading(Vcp::INPUT_SOURCE, 0x0F0F, 0x001B));
    }

    #[test]
    fn resolves_symbolic_values() {
        assert_eq!(resolve_value(0x60, "usb-c"), Some(0x1B));
        assert_eq!(resolve_value(0x60, "dp"), Some(0x0F));
        assert_eq!(resolve_value(0x60, "dp2"), Some(0x13));
        assert_eq!(resolve_value(0xE9, "off"), Some(0x00));
        assert_eq!(resolve_value(0xE9, "pip"), Some(0x21));
        assert_eq!(resolve_value(0xE9, "quad"), Some(0x41));
        assert_eq!(resolve_value(0xD6, "standby"), Some(0x04));
        assert_eq!(resolve_value(0x10, "75"), Some(75));
        assert_eq!(resolve_value(0xE9, "2up-right"), None);
    }

    #[test]
    fn write_aliases_depend_on_the_current_layout() {
        use pip::Effect;
        assert_eq!(pip::canonical(0x01), pip::PBP_L2R1);
        assert_eq!(pip::canonical(0x02), pip::PBP_2UP_SELF_LEFT);
        assert_eq!(pip::canonical(0x21), 0x21);

        assert_eq!(
            pip::write_effect(pip::OFF, 0x01),
            Effect::Layout(pip::PBP_L2R1)
        );
        assert_eq!(
            pip::write_effect(pip::OFF, 0x02),
            Effect::Layout(pip::PBP_2UP_SELF_LEFT)
        );
        for from in [pip::PIP_SMALL, pip::PIP_LARGE] {
            assert_eq!(pip::write_effect(from, 0x01), Effect::StepInsetSize);
            assert_eq!(pip::write_effect(from, 0x02), Effect::StepInsetCorner);
        }
        for from in [pip::OFF, pip::PIP_SMALL, pip::QUAD_SELF_LEFT_COLUMN] {
            assert_eq!(
                pip::write_effect(from, pip::PBP_L2R1),
                Effect::Layout(pip::PBP_L2R1)
            );
        }
    }

    #[test]
    fn aliases_are_legal_but_not_canonical() {
        assert_eq!(pip::CANONICAL.len(), pip::ALL_LEGAL.len() - 2);
        for v in [pip::ALIAS_LR_SELF_LEFT, pip::ALIAS_LR_SELF_RIGHT] {
            assert!(pip::ALL_LEGAL.contains(&v));
            assert!(!pip::CANONICAL.contains(&v));
        }
    }

    #[test]
    fn decodes_captured_sub_source_words() {
        use pip_sub::decode;
        assert_eq!(
            decode(0x6DF1),
            [input::HDMI_1, input::DISPLAY_PORT, input::USB_C]
        );
        assert_eq!(
            decode(0x6DF3),
            [input::DP2, input::DISPLAY_PORT, input::USB_C]
        );
        assert_eq!(
            decode(0x6DF2),
            [input::HDMI_2, input::DISPLAY_PORT, input::USB_C]
        );
    }

    #[test]
    fn predicts_an_accepted_sub_source_word() {
        assert_eq!(pip_sub::with_sub1(0x6DF3, input::DISPLAY_PORT), 0x6DEF);
        assert_eq!(pip_sub::sub1_of(0x6DEF), input::DISPLAY_PORT);
    }

    #[test]
    fn sub_source_round_trips() {
        let subs = [input::HDMI_2, input::DISPLAY_PORT, input::USB_C];
        assert_eq!(pip_sub::encode(subs), 0x6DF2);
        assert_eq!(pip_sub::decode(0x6DF2), subs);
    }

    // The constants here and the profile TOML must agree.
    #[test]
    fn pip_constants_match_the_profile() {
        let pairs = [
            (pip::OFF, "off"),
            (pip::PIP_SMALL, "pip-small"),
            (pip::PIP_LARGE, "pip-large"),
            (pip::PBP_2UP_SELF_LEFT, "pbp-2up-self-left"),
            (pip::PBP_L2R1, "pbp-l2r1"),
            (pip::PBP_3UP_LEFT_PLUS_STACK, "pbp-3up-left-stack"),
            (pip::PBP_2UP_VERTICAL_SELF_TOP, "pbp-vsplit-self-top"),
            (pip::PBP_VSTACK_SELF_TOP, "pbp-vstack-self-top"),
            (pip::PBP_T2B1, "pbp-t2b1"),
            (pip::SPLIT_3UP_SELF_LEFT, "pbp-3up"),
            (pip::QUAD_SELF_LEFT_COLUMN, "pbp-quad"),
        ];
        for (value, name) in pairs {
            assert_eq!(
                describe_value(Vcp::PIP_MODE, value),
                Some(name),
                "0x{value:02X}"
            );
        }
    }

    #[test]
    fn input_constants_match_the_profile() {
        for (value, name) in [
            (input::DISPLAY_PORT, "dp1"),
            (input::DP2, "dp2"),
            (input::HDMI_1, "hdmi1"),
            (input::HDMI_2, "hdmi2"),
            (input::USB_C, "usb-c"),
        ] {
            assert_eq!(describe_value(Vcp::INPUT_SOURCE, value), Some(name));
        }
    }
}
