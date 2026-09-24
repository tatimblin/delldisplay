//! Preconditions, checked before any write.
//!
//! The panel drops a lot of writes silently while reporting success: a
//! sub-window the active 0xE9 layout doesn't have, anything at all while the OSD
//! is open, a 0xDC value the capability list doesn't advertise. From the host
//! they all look the same, so a guard turns each one into a sentence.
//!
//! Every guard reads one [`Snapshot`] taken up front and nothing here issues
//! traffic, so a refusal can truthfully say nothing changed.

use std::cmp::Reverse;
use std::fmt;

use crate::bits;
use crate::caps::Capabilities;
use crate::plan::Plan;
use crate::vcp::{input_label, pip, Panel, Vcp};

/// What a guard concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing stands in the way.
    Allow,
    /// Go ahead, but tell the user this first.
    Warn(String),
    /// Don't write. The string is the whole explanation the user gets.
    Refuse(String),
}

impl Verdict {
    pub fn is_refusal(&self) -> bool {
        matches!(self, Verdict::Refuse(_))
    }
    pub fn is_allow(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
    pub fn message(&self) -> Option<&str> {
        match self {
            Verdict::Allow => None,
            Verdict::Warn(m) | Verdict::Refuse(m) => Some(m),
        }
    }
    /// 0 allow, 1 warn, 2 refuse.
    pub fn severity(&self) -> u8 {
        match self {
            Verdict::Allow => 0,
            Verdict::Warn(_) => 1,
            Verdict::Refuse(_) => 2,
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verdict::Allow => write!(f, "allow"),
            Verdict::Warn(m) => write!(f, "warning: {m}"),
            Verdict::Refuse(m) => write!(f, "refused: {m}"),
        }
    }
}

/// Drop `Allow`s and duplicates, most severe first. Order within a severity is
/// kept.
pub fn tidy(verdicts: Vec<Verdict>) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = Vec::with_capacity(verdicts.len());
    for v in verdicts {
        if !v.is_allow() && !out.contains(&v) {
            out.push(v);
        }
    }
    out.sort_by_key(|v| Reverse(v.severity()));
    out
}

/// The refusal messages. Empty means the write may go ahead.
pub fn refusals_of(verdicts: &[Verdict]) -> Vec<String> {
    verdicts
        .iter()
        .filter_map(|v| match v {
            Verdict::Refuse(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

/// The warning messages.
pub fn warnings_of(verdicts: &[Verdict]) -> Vec<String> {
    verdicts
        .iter()
        .filter_map(|v| match v {
            Verdict::Warn(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

/// `1B 0F 13`, the way the capability string spells a value list.
pub fn hex_list(values: &[u8]) -> String {
    values
        .iter()
        .map(|v| format!("{v:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The panel state a guard may look at, read once per command.
///
/// Every register is optional because a read can fail, and a failed read must
/// not turn into a silent allow: a guard that can't see what it needs warns.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The profile names and quirks come from.
    pub panel: &'static Panel,
    pub caps: Capabilities,
    /// 0xF2 status.
    pub status: Option<u16>,
    /// 0xF1 feature bitmask.
    pub features: Option<u16>,
    /// 0x60 input source, both bytes.
    pub input: Option<u16>,
    /// 0xE9 PiP/PBP layout.
    pub layout: Option<u16>,
    /// 0xE8 packed sub-sources.
    pub sub: Option<u16>,
    /// 0xE7 USB KVM association word.
    pub kvm: Option<u16>,
    /// 0xEE USB upstream port inventory.
    pub ports: Option<u16>,
}

/// The codes a snapshot reads, in order: 0xF2 first so a busy panel is found
/// before spending more reads, and 0xE9 before 0xE8 because the layout is what
/// makes 0xE8's fields mean anything.
pub const READ_ORDER: [u8; 7] = [
    Vcp::STATUS,
    Vcp::FEATURES,
    Vcp::PIP_MODE,
    Vcp::INPUT_SOURCE,
    Vcp::PORT_INVENTORY,
    Vcp::USB_KVM,
    Vcp::PIP_SUB_SOURCE,
];

impl Snapshot {
    pub fn new(panel: &'static Panel, caps: Capabilities) -> Self {
        Snapshot {
            panel,
            caps,
            status: None,
            features: None,
            input: None,
            layout: None,
            sub: None,
            kvm: None,
            ports: None,
        }
    }

    /// Fold `(vcp, value)` reads into a snapshot. Missing codes stay `None`.
    pub fn from_reads(panel: &'static Panel, caps: Capabilities, reads: &[(u8, u16)]) -> Self {
        let mut s = Snapshot::new(panel, caps);
        for (code, value) in reads {
            s.put(*code, *value);
        }
        s
    }

    /// Record one read.
    pub fn put(&mut self, code: u8, value: u16) {
        match code {
            Vcp::STATUS => self.status = Some(value),
            Vcp::FEATURES => self.features = Some(value),
            Vcp::INPUT_SOURCE => self.input = Some(value),
            Vcp::PIP_MODE => self.layout = Some(value),
            Vcp::PIP_SUB_SOURCE => self.sub = Some(value),
            Vcp::USB_KVM => self.kvm = Some(value),
            Vcp::PORT_INVENTORY => self.ports = Some(value),
            _ => {}
        }
    }

    /// The input the main window shows: 0x60's low byte.
    pub fn main_input(&self) -> Option<u8> {
        self.input.map(|w| bits::input_word(w).selected)
    }

    /// The decoded 0x60 word, high byte included.
    pub fn input_word(&self) -> Option<bits::InputWord> {
        self.input.map(bits::input_word)
    }

    /// The active layout, write-aliases folded.
    pub fn layout_code(&self) -> Option<u8> {
        self.layout.map(|w| pip::canonical((w & 0xFF) as u8))
    }

    /// Pane counts for the active layout.
    pub fn panes(&self) -> Option<bits::Panes> {
        self.layout_code().map(bits::panes)
    }

    /// The three sub-window source codes from 0xE8.
    pub fn sub_sources(&self) -> Option<[u8; 3]> {
        self.sub.map(crate::vcp::pip_sub::decode)
    }

    pub fn status_bits(&self) -> Option<bits::Status> {
        self.status.map(bits::status)
    }

    pub fn feature_bits(&self) -> Option<bits::Features> {
        self.features.map(bits::features)
    }
}

/// What the caller is about to do: one variant per user-facing operation, since
/// that's what a refusal has to explain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Change the main window's source (0x60).
    SetInput { input: u8 },
    /// Change the PiP/PBP layout (0xE9).
    SetLayout { layout: u8 },
    /// Point a sub-window at an input (0xE8). `window` is a window index, so
    /// main is 0 and a sub-window is 1, 2 or 3.
    SetSubSource { window: u8, input: u8 },
    /// Store an upstream slot in the USB association map (0xE7). Switches
    /// nothing.
    KvmAssociate { slot: u8 },
    /// Move USB to the next upstream (0xE7 = 0xFF00).
    KvmToggle,
    /// Exchange two windows' sources (0xE5 = 0xF0ab).
    PxpSwap { a: u8, b: u8 },
    /// PBP zoom step (0xE5 = 0x0002).
    Zoom,
    /// Underscan toggle (0xE5 = 0x0003).
    Underscan,
    /// Power on and clear the PowerNap flags.
    Wake,
    /// Anything else. Checked for busy, capability and the advertised values.
    Raw { vcp: u8, value: u16 },
}

impl Intent {
    /// VCP codes that must be advertised for this intent to be possible.
    pub fn required_codes(&self) -> Vec<u8> {
        match *self {
            Intent::SetInput { .. } => vec![Vcp::INPUT_SOURCE],
            Intent::SetLayout { .. } => vec![Vcp::PIP_MODE],
            Intent::SetSubSource { .. } => vec![Vcp::PIP_SUB_SOURCE, Vcp::PIP_MODE],
            Intent::KvmAssociate { .. } => vec![Vcp::USB_KVM, Vcp::PORT_INVENTORY],
            Intent::KvmToggle => vec![Vcp::USB_KVM],
            Intent::PxpSwap { .. } => vec![Vcp::PXP_ACTION, Vcp::PIP_MODE],
            Intent::Zoom | Intent::Underscan => vec![Vcp::PXP_ACTION],
            Intent::Wake => vec![Vcp::POWER_MODE, Vcp::POWERNAP_DIM, Vcp::POWERNAP_SLEEP],
            Intent::Raw { vcp, .. } => vec![vcp],
        }
    }
}

/// The 0xF2 busy check: refuse while the OSD is open, warn if 0xF2 couldn't be
/// read, `None` when the panel is idle.
pub fn busy(status: Option<u16>) -> Option<Verdict> {
    match status.map(bits::status) {
        Some(s) if s.busy() => Some(Verdict::Refuse(format!(
            "the panel is busy (0xF2 = 0x{:04X}, bit 7 set). Close the on-screen menu and \
             try again; nothing was written",
            s.raw
        ))),
        Some(_) => None,
        None => Some(Verdict::Warn(String::from(
            "couldn't read the busy flag (0xF2); if the on-screen menu is open the write may \
             be dropped",
        ))),
    }
}

/// Every guard with something to say about `intent`, most severe first. Empty
/// means go ahead.
pub fn evaluate(snap: &Snapshot, intent: &Intent) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = busy(snap.status).into_iter().collect();
    for code in intent.required_codes() {
        if !snap.caps.supports(code) {
            out.push(Verdict::Refuse(format!(
                "this panel doesn't advertise VCP 0x{code:02X}; it advertises: {}",
                advertised(&snap.caps)
            )));
        }
    }
    out.extend(match *intent {
        Intent::SetInput { input } => set_input(snap, input),
        Intent::SetLayout { layout } => check_enumerated(snap, Vcp::PIP_MODE, layout)
            .into_iter()
            .collect(),
        Intent::SetSubSource { window, input } => set_sub_source(snap, window, input),
        Intent::KvmAssociate { slot } => kvm_associate(snap, slot),
        Intent::PxpSwap { a, b } => pxp_swap(snap, a, b),
        Intent::Zoom => zoom_gate(snap, "PBP zoom"),
        Intent::Underscan => zoom_gate(snap, "underscan"),
        Intent::KvmToggle | Intent::Wake => Vec::new(),
        Intent::Raw { vcp, value } => raw_value(snap, vcp, value),
    });
    tidy(out)
}

/// The single most severe verdict, or [`Verdict::Allow`].
pub fn check(snap: &Snapshot, intent: &Intent) -> Verdict {
    evaluate(snap, intent)
        .into_iter()
        .next()
        .unwrap_or(Verdict::Allow)
}

/// Busy and capability checks for a whole [`Plan`]: every code it writes must
/// be advertised.
pub fn check_plan(snap: &Snapshot, plan: &Plan) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = busy(snap.status).into_iter().collect();
    for code in plan.written_codes() {
        if !snap.caps.supports(code) {
            out.push(Verdict::Refuse(format!(
                "'{}' writes VCP 0x{code:02X}, which this panel doesn't advertise; it \
                 advertises: {}",
                plan.name,
                advertised(&snap.caps)
            )));
        }
    }
    tidy(out)
}

/// "`X` is also ..., so it will appear twice". Mirroring is legal on this
/// panel; it shows the source in both cells.
pub(crate) fn mirrored(snap: &Snapshot, input: u8, also: &str) -> Verdict {
    Verdict::Warn(format!(
        "{} is also {also}, so it will appear twice (mirrored, not moved)",
        input_label(snap.panel, input)
    ))
}

fn advertised(caps: &Capabilities) -> String {
    if caps.vcp.is_empty() {
        return String::from("(the capability string is empty or wasn't read)");
    }
    hex_list(&caps.vcp.iter().map(|e| e.code).collect::<Vec<_>>())
}

/// Refuse a value the capability string enumerates and doesn't contain.
fn check_enumerated(snap: &Snapshot, code: u8, value: u8) -> Option<Verdict> {
    let vals = snap.caps.legal_values(code)?;
    if vals.is_empty() || vals.contains(&value) {
        return None;
    }
    Some(Verdict::Refuse(format!(
        "0x{value:02X} isn't a legal value for VCP 0x{code:02X} on this panel; it advertises: {}",
        hex_list(vals)
    )))
}

fn set_input(snap: &Snapshot, input: u8) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = check_enumerated(snap, Vcp::INPUT_SOURCE, input)
        .into_iter()
        .collect();
    if let (Some(subs), Some(panes)) = (snap.sub_sources(), snap.panes()) {
        for (window, s) in (1u8..).zip(subs) {
            if s == input && s != 0 && panes.has_window(window) {
                out.push(mirrored(snap, input, &format!("in sub-window {window}")));
            }
        }
    }
    if snap.main_input() == Some(input) {
        out.push(Verdict::Warn(format!(
            "the main window is already on {}",
            input_label(snap.panel, input)
        )));
    }
    out
}

fn set_sub_source(snap: &Snapshot, window: u8, input: u8) -> Vec<Verdict> {
    if window == 0 {
        return vec![Verdict::Refuse(String::from(
            "window 0 is the main window; its source is VCP 0x60, not 0xE8",
        ))];
    }
    if window > 3 {
        return vec![Verdict::Refuse(format!(
            "0xE8 has three sub-window fields; there's no window {window}"
        ))];
    }
    let mut out = Vec::new();
    if snap.main_input() == Some(input) {
        out.push(mirrored(snap, input, "the main window's source"));
    }
    if input != 0 {
        out.extend(check_enumerated(snap, Vcp::INPUT_SOURCE, input));
    }
    match snap.layout_code() {
        Some(l) => {
            let panes = bits::panes(l);
            if !panes.has_window(window) {
                out.push(Verdict::Refuse(format!(
                    "the active layout 0x{l:02X} has {} window(s), so there's no sub-window \
                     {window} to assign. Change the layout first.",
                    panes.bound()
                )));
            } else if !panes.proven() {
                out.push(Verdict::Warn(format!(
                    "layout 0x{l:02X}'s pane count ({}) is unmeasured; allowing window \
                     {window}, but the panel may ignore it",
                    panes.bound()
                )));
            }
        }
        None => out.push(Verdict::Warn(String::from(
            "couldn't read the layout (0xE9), so the sub-window count is unknown",
        ))),
    }
    out
}

fn kvm_associate(snap: &Snapshot, slot: u8) -> Vec<Verdict> {
    if slot > 3 {
        return vec![Verdict::Refuse(
            crate::kvm::AssocError::SlotOutOfRange { slot }.to_string(),
        )];
    }
    match snap.ports {
        Some(_) => Vec::new(),
        None => vec![Verdict::Warn(String::from(
            "couldn't read the port inventory (0xEE); a slot is an index into it, so the \
             port it names is unknown",
        ))],
    }
}

fn pxp_swap(snap: &Snapshot, a: u8, b: u8) -> Vec<Verdict> {
    let mut out = Vec::new();
    if a == b {
        out.push(Verdict::Refuse(String::from(
            "swapping a window with itself does nothing",
        )));
    }
    match snap.layout_code() {
        Some(l) => {
            let panes = bits::panes(l);
            if panes.bound() < 2 {
                out.push(Verdict::Refuse(String::from(
                    "PiP/PBP is off, so there's nothing to swap with. Set a layout first.",
                )));
            } else {
                for w in [a, b].into_iter().filter(|w| !panes.has_window(*w)) {
                    out.push(Verdict::Refuse(format!(
                        "the active layout 0x{l:02X} has {} window(s); there's no window {w} \
                         to swap",
                        panes.bound()
                    )));
                }
            }
        }
        None => out.push(Verdict::Warn(String::from(
            "couldn't read the layout (0xE9), so the window numbers weren't checked",
        ))),
    }
    if !matches!((a, b), (0, 1) | (1, 0)) {
        out.push(Verdict::Warn(String::from(
            "only the main<->sub1 swap (0xF010) is verified; other pairs use the same \
             encoding but are inferred",
        )));
    }
    out
}

fn zoom_gate(snap: &Snapshot, what: &str) -> Vec<Verdict> {
    let warn = match snap.feature_bits() {
        Some(f) if f.is_read_failure() => {
            format!("0xF1 read 0xFFFF, the read-failure value, so the {what} gate wasn't checked")
        }
        // Dell's software hides the control when bit 5 is clear. The bit is set
        // on the U4323QE and both actions do nothing there, so it's no evidence
        // either way.
        Some(f) if !f.zoom_underscan() => format!(
            "0xF1 = 0x{:04X} has bit 5 clear, which Dell's software treats as no {what}. \
             The bit hasn't predicted anything on a real panel, so this isn't a refusal.",
            f.raw
        ),
        Some(_) => return Vec::new(),
        None => format!("couldn't read 0xF1, so the {what} gate (bit 5) wasn't checked"),
    };
    vec![Verdict::Warn(warn)]
}

fn raw_value(snap: &Snapshot, vcp: u8, value: u16) -> Vec<Verdict> {
    match snap.caps.legal_values(vcp) {
        Some(vals) if !vals.is_empty() && value <= 0xFF && !vals.contains(&(value as u8)) => {
            vec![Verdict::Warn(format!(
                "0x{value:02X} isn't in the values this panel advertises for 0x{vcp:02X}: {}",
                hex_list(vals)
            ))]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::idle_snapshot as live;
    use crate::vcp::input;

    fn refusals(snap: &Snapshot, intent: &Intent) -> Vec<String> {
        refusals_of(&evaluate(snap, intent))
    }

    #[test]
    fn snapshot_decodes_every_register() {
        let s = live();
        assert_eq!(s.main_input(), Some(input::USB_C));
        assert_eq!(
            s.input_word().unwrap().arrival,
            bits::Arrival::Port(input::USB_C)
        );
        assert_eq!(s.layout_code(), Some(0x24));
        assert_eq!(s.panes().unwrap().bound(), 2);
        assert_eq!(
            s.sub_sources(),
            Some([input::HDMI_1, input::DISPLAY_PORT, input::USB_C])
        );
        assert!(!s.status_bits().unwrap().busy());
        assert!(s.feature_bits().unwrap().zoom_underscan());
    }

    #[test]
    fn read_order_puts_layout_before_sub_sources() {
        let e9 = READ_ORDER.iter().position(|c| *c == 0xE9).unwrap();
        let e8 = READ_ORDER.iter().position(|c| *c == 0xE8).unwrap();
        assert!(e9 < e8);
        assert_eq!(READ_ORDER[0], 0xF2);
    }

    #[test]
    fn busy_panel_refuses_everything() {
        let mut s = live();
        s.status = Some(0x0080);
        for intent in [
            Intent::SetInput { input: input::DP2 },
            Intent::SetLayout { layout: 0x21 },
            Intent::SetSubSource {
                window: 1,
                input: input::DP2,
            },
            Intent::KvmAssociate { slot: 1 },
            Intent::KvmToggle,
            Intent::PxpSwap { a: 0, b: 1 },
            Intent::Zoom,
            Intent::Underscan,
            Intent::Wake,
            Intent::Raw {
                vcp: 0x10,
                value: 50,
            },
        ] {
            let v = check(&s, &intent);
            assert!(v.is_refusal(), "{intent:?} should be refused while busy");
            assert!(v.message().unwrap().contains("busy"));
            assert!(v.message().unwrap().contains("nothing was written"));
        }
    }

    #[test]
    fn the_busy_check_refuses_warns_and_allows() {
        assert!(busy(Some(0x0080)).unwrap().is_refusal());
        assert_eq!(busy(Some(0x0000)), None);
        assert!(matches!(busy(None), Some(Verdict::Warn(_))));
    }

    #[test]
    fn unreadable_status_warns_rather_than_allowing() {
        let mut s = live();
        s.status = None;
        let v = check(&s, &Intent::SetInput { input: input::DP2 });
        assert!(matches!(v, Verdict::Warn(_)));
        assert!(v.message().unwrap().contains("0xF2"));
    }

    #[test]
    fn unadvertised_code_is_refused_with_the_advertised_list() {
        // 0x63 (audio source) isn't in this panel's capability string.
        let v = check(
            &live(),
            &Intent::Raw {
                vcp: 0x63,
                value: 0xF2,
            },
        );
        assert!(v.is_refusal());
        let m = v.message().unwrap();
        assert!(m.contains("0x63"));
        assert!(m.contains("60"), "the advertised list must be listed: {m}");
        assert!(m.contains("E9"));
    }

    #[test]
    fn main_input_equal_to_an_assigned_sub_warns_but_is_allowed() {
        let v = check(
            &live(),
            &Intent::SetInput {
                input: input::HDMI_1,
            },
        );
        assert!(!v.is_refusal(), "{v:?}");
        assert!(v.message().unwrap().contains("appear twice"), "{v:?}");
    }

    #[test]
    fn a_sub_equal_to_the_main_input_warns_but_is_allowed() {
        // live() has main on usb-c.
        let v = check(
            &live(),
            &Intent::SetSubSource {
                window: 1,
                input: input::USB_C,
            },
        );
        assert!(!v.is_refusal(), "{v:?}");
        assert!(v.message().unwrap().contains("appear twice"), "{v:?}");
    }

    #[test]
    fn a_sub_beyond_the_layouts_pane_count_is_refused() {
        let s = live(); // 0x24: windows 0 and 1.
        assert!(check(
            &s,
            &Intent::SetSubSource {
                window: 1,
                input: input::DP2
            }
        )
        .is_allow());
        let v = check(
            &s,
            &Intent::SetSubSource {
                window: 2,
                input: input::DP2,
            },
        );
        assert!(v.is_refusal());
        assert!(v.message().unwrap().contains("0x24"));
        assert!(v.message().unwrap().contains("Change the layout first"));
    }

    #[test]
    fn three_pane_layouts_allow_their_third_window_silently() {
        for layout in [0x0032u16, 0x0033, 0x0035] {
            let mut s = live();
            s.layout = Some(layout);
            let v = evaluate(
                &s,
                &Intent::SetSubSource {
                    window: 2,
                    input: input::DP2,
                },
            );
            assert!(v.is_empty(), "0x{layout:02X}: {v:?}");
            assert!(check(
                &s,
                &Intent::SetSubSource {
                    window: 3,
                    input: input::DP2
                }
            )
            .is_refusal());
        }
        let mut s = live();
        s.layout = Some(0x0041);
        assert!(!check(
            &s,
            &Intent::SetSubSource {
                window: 3,
                input: input::DP2
            }
        )
        .is_refusal());
    }

    #[test]
    fn a_window_outside_0xe8_is_refused_once() {
        let s = live();
        for window in [0, 4] {
            let r = refusals(
                &s,
                &Intent::SetSubSource {
                    window,
                    input: input::DP2,
                },
            );
            assert_eq!(r.len(), 1, "window {window}: {r:?}");
        }
        assert!(refusals(
            &s,
            &Intent::SetSubSource {
                window: 0,
                input: input::DP2
            }
        )[0]
        .contains("0x60"));
    }

    #[test]
    fn a_clear_f1_bit5_warns_but_does_not_refuse() {
        let mut s = live();
        s.features = Some(0xC12B & !0x20);
        for intent in [Intent::Zoom, Intent::Underscan] {
            let v = check(&s, &intent);
            assert!(!v.is_refusal(), "{v:?}");
            assert!(v.message().unwrap().contains("bit 5"), "{v:?}");
        }
    }

    #[test]
    fn zoom_with_bit5_set_has_nothing_to_say() {
        assert!(evaluate(&live(), &Intent::Zoom).is_empty());
    }

    #[test]
    fn illegal_enumerated_values_are_refused() {
        let s = live();
        // 0x10 is not in 60(1B 0F 13 11 12).
        let v = check(&s, &Intent::SetInput { input: 0x10 });
        assert!(v.is_refusal());
        assert!(v.message().unwrap().contains("isn't a legal value"));
        assert!(check(&s, &Intent::SetLayout { layout: 0x99 }).is_refusal());
        // The write-aliases 0x01/0x02 are advertised.
        assert!(!check(&s, &Intent::SetLayout { layout: 0x02 }).is_refusal());
    }

    #[test]
    fn an_association_slot_must_fit_in_two_bits() {
        let s = live();
        assert!(evaluate(&s, &Intent::KvmAssociate { slot: 3 }).is_empty());
        assert_eq!(refusals(&s, &Intent::KvmAssociate { slot: 4 }).len(), 1);
    }

    #[test]
    fn always_true_facts_are_not_repeated_as_warnings() {
        let s = live();
        for intent in [
            Intent::KvmToggle,
            Intent::Wake,
            Intent::Zoom,
            Intent::Underscan,
        ] {
            assert!(evaluate(&s, &intent).is_empty(), "{intent:?}");
        }
    }

    #[test]
    fn swap_needs_two_windows() {
        let mut s = live();
        s.layout = Some(0x0000);
        let v = check(&s, &Intent::PxpSwap { a: 0, b: 1 });
        assert!(v.is_refusal());
        assert!(v.message().unwrap().contains("nothing"));

        s.layout = Some(0x0024);
        assert!(!check(&s, &Intent::PxpSwap { a: 0, b: 1 }).is_refusal());
        assert!(check(&s, &Intent::PxpSwap { a: 1, b: 1 }).is_refusal());
        assert!(check(&s, &Intent::PxpSwap { a: 0, b: 3 }).is_refusal());
    }

    #[test]
    fn unverified_swap_pairs_are_warned_about() {
        let mut s = live();
        s.layout = Some(0x0041);
        let v = check(&s, &Intent::PxpSwap { a: 1, b: 2 });
        assert!(!v.is_refusal());
        assert!(v.message().unwrap().contains("inferred"));
        assert!(check(&s, &Intent::PxpSwap { a: 0, b: 1 }).is_allow());
    }

    #[test]
    fn the_ordinary_case_is_allowed() {
        let s = live();
        assert_eq!(
            check(&s, &Intent::SetInput { input: input::DP2 }),
            Verdict::Allow
        );
        assert_eq!(
            check(&s, &Intent::SetLayout { layout: 0x21 }),
            Verdict::Allow
        );
        assert!(refusals(&s, &Intent::SetInput { input: input::DP2 }).is_empty());
    }

    #[test]
    fn check_plan_catches_unadvertised_writes_and_a_busy_panel() {
        let s = live();
        let ok = Plan::new("ok").write(0x60, 0x1B).write(0xE9, 0x24);
        assert!(check_plan(&s, &ok).is_empty());

        let bad = Plan::new("bad").write(0x63, 0x00F1);
        let r = refusals_of(&check_plan(&s, &bad));
        assert!(r[0].contains("0x63"));

        let mut busy_panel = live();
        busy_panel.status = Some(0x0080);
        assert!(refusals_of(&check_plan(&busy_panel, &ok))[0].contains("busy"));
    }

    #[test]
    fn check_plan_warns_when_the_busy_flag_is_unreadable() {
        let mut blind = live();
        blind.status = None;
        let v = check_plan(&blind, &Plan::new("swap").write(0xE5, 0xF010));
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Verdict::Warn(m) if m.contains("0xF2")));
    }

    #[test]
    fn tidy_dedups_and_sorts_most_severe_first() {
        let w = Verdict::Warn("w".into());
        let r = Verdict::Refuse("r".into());
        let v = tidy(vec![
            w.clone(),
            Verdict::Allow,
            r.clone(),
            w.clone(),
            r.clone(),
        ]);
        assert_eq!(v, vec![r, w]);
        assert_eq!(refusals_of(&v), vec!["r"]);
        assert_eq!(warnings_of(&v), vec!["w"]);
    }

    #[test]
    fn verdicts_are_sorted_most_severe_first() {
        let mut s = live();
        s.status = Some(0x0080);
        s.features = None;
        let v = evaluate(&s, &Intent::Zoom);
        assert!(v[0].is_refusal());
        assert!(v.len() > 1);
    }
}
