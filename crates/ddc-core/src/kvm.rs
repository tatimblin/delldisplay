//! VCP 0xE7, the USB KVM.
//!
//! One code, three meanings:
//!
//! * Toggle: `0xE7 = 0xFF00` moves the USB hub to the next upstream. It's an
//!   action, so the register keeps reading its old value. Verified on the
//!   U4323QE.
//! * Window bind: `0xFF01..=0xFF04` binds USB to the host in PxP window 1..4.
//!   Seen only in Dell's software, which offers it only when `FF` is in the
//!   capability string's `E7(...)` list. [`check_bind_window`] refuses it
//!   otherwise.
//! * Association map: any other value is a stored map of which upstream the
//!   panel attaches when each input is active. Writing it switches nothing.
//!
//! In the map, only bits 6..13 are writable (the panel drops the rest), and a
//! field holds an index into the 0xEE port inventory, not a port code. Which
//! input owns which field is unresolved; see [`FieldOrder`].
//!
//! Nothing here issues traffic.

use std::fmt;
use std::time::Duration;

use crate::bits::{self, Arrival, InputWord, Panes};
use crate::caps::Capabilities;
use crate::guard::{self, hex_list, Intent, Snapshot, Verdict};
use crate::plan::Plan;
use crate::vcp::{input_label, input_name, pip_sub, Panel, Vcp};

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Move USB to the next upstream. An action, not a stored value.
pub const TOGGLE: u16 = 0xFF00;

/// What Dell's software needs in `E7(...)` before it offers the window bind.
const WINDOW_BIND_MARKER: u8 = 0xFF;

/// The highest PxP window the bind can name (`0xFF04`).
const MAX_BIND_WINDOW: u8 = 4;

/// The 0xE7 value that binds USB to PxP window `window` (1..=4). Inferred from
/// Dell's software.
pub fn bind_window_value(window: u8) -> Option<u16> {
    (1..=MAX_BIND_WINDOW)
        .contains(&window)
        .then_some(0xFF00u16 | window as u16)
}

/// Whether `FF` is in the capability string's `E7(...)` list. An empty or
/// missing list doesn't count.
fn window_bind_advertised(caps: &Capabilities) -> bool {
    caps.legal_values(Vcp::USB_KVM)
        .is_some_and(|v| v.contains(&WINDOW_BIND_MARKER))
}

// ---------------------------------------------------------------------------
// Map layout
// ---------------------------------------------------------------------------

/// Writable region, bits 6..13. The panel silently drops writes to 0-5 and
/// 14-15.
const WRITABLE_MASK: u16 = 0x3FC0;

/// The four writable 2-bit fields, low bit first. Says which bits move, not
/// which input owns them.
const FIELD_SHIFTS: [u8; 4] = [6, 8, 10, 12];

/// Which input owns which pair of bits.
///
/// Unresolved. Both orders fit every measurement; they agree only at index 2
/// (bit 10), the one index tested. Flip [`FIELD_ORDER`] to switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrder {
    /// Input 0 at bit 6, climbing.
    LowFirst,
    /// Input 0 at bit 14, descending. Dell's encoder.
    DdpmHighFirst,
}

/// The field order every path in this module uses.
pub const FIELD_ORDER: FieldOrder = FieldOrder::LowFirst;

impl FieldOrder {
    /// Low bit of input `index`'s field, before the writability check.
    const fn shift_for_input(self, index: usize) -> Option<u8> {
        match self {
            FieldOrder::LowFirst if index < 4 => Some(6 + 2 * index as u8),
            FieldOrder::DdpmHighFirst if index < 8 => Some(14 - 2 * index as u8),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            FieldOrder::LowFirst => "low-first (input 0 at bit 6)",
            FieldOrder::DdpmHighFirst => "high-first (input 0 at bit 14, Dell's)",
        }
    }
}

impl fmt::Display for FieldOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The order Dell's software would use here. It calls any panel whose
/// capability string contains "EE" an EE panel and packs those high-first.
fn ddpm_order(caps: &Capabilities) -> Option<FieldOrder> {
    caps.raw.contains("EE").then_some(FieldOrder::DdpmHighFirst)
}

/// True when a 2-bit field at `shift` sits entirely inside [`WRITABLE_MASK`].
const fn shift_is_writable(shift: u8) -> bool {
    shift <= 14 && (0x3u16 << shift) & WRITABLE_MASK == (0x3u16 << shift)
}

/// The writable shift for capability-input `index` under [`FIELD_ORDER`].
fn writable_shift_for_input(index: usize) -> Option<u8> {
    FIELD_ORDER
        .shift_for_input(index)
        .filter(|s| shift_is_writable(*s))
}

/// The four field values of a 0xE7 word, bit 6 first. Says what the bits hold,
/// not which input owns them.
pub fn decode(word: u16) -> [u8; 4] {
    FIELD_SHIFTS.map(|sh| ((word >> sh) & 0x3) as u8)
}

/// Set one 2-bit field and leave every other bit alone.
fn put_field(word: u16, shift: u8, value: u8) -> u16 {
    (word & !(0x3 << shift)) | (((value as u16) & 0x3) << shift)
}

/// A field value indexes the panel's port inventory (0xEE). On the U4323QE,
/// 0xEE reads 0xBA98: four USB-C slots and no USB-B.
pub mod port {
    /// Names for the codes in 0xEE's nibbles.
    pub fn nibble_name(code: u8) -> &'static str {
        match code {
            0x0 => "usb-b1",
            0x1 => "usb-b2",
            0x8 => "usb-c1",
            0x9 => "usb-c2",
            0xA => "usb-c3",
            0xB => "usb-c4",
            0xC => "thunderbolt",
            0xD => "thunderbolt2",
            _ => "?",
        }
    }

    /// The four inventory slots, low nibble first.
    pub fn inventory(ee_word: u16) -> [u8; 4] {
        [0, 1, 2, 3].map(|i| ((ee_word >> (i * 4)) & 0xF) as u8)
    }

    /// Name the port a field value points at.
    pub fn resolve(ee_word: u16, field_value: u8) -> &'static str {
        inventory(ee_word)
            .get(field_value as usize)
            .map(|c| nibble_name(*c))
            .unwrap_or("?")
    }

    /// The slot holding a named port (`usb-c2`), if there is one.
    pub fn slot_named(ee_word: u16, name: &str) -> Option<u8> {
        let want = name.trim().to_ascii_lowercase();
        inventory(ee_word)
            .iter()
            .position(|c| nibble_name(*c) == want)
            .map(|i| i as u8)
    }
}

// ---------------------------------------------------------------------------
// The map as a value
// ---------------------------------------------------------------------------

/// Why an association or commit word couldn't be built. Nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssocError {
    /// 0xE7 wasn't read, and the map is read-modify-write.
    MapUnread,
    /// 0xE8 wasn't read, and a commit rewrites all three sub-sources.
    SubUnread,
    /// The capability string's `60(...)` list wasn't read.
    NoInputList,
    /// The panel doesn't list that input.
    UnknownInput { input: u8, inputs: Vec<u8> },
    /// The input is listed but its field is outside the writable bits.
    NoField { input: u8, index: usize },
    /// A field is two bits wide.
    SlotOutOfRange { slot: u8 },
}

impl fmt::Display for AssocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssocError::MapUnread => write!(
                f,
                "couldn't read 0xE7, and the association map is read-modify-write: writing it \
                 blind would wipe every input's upstream. Nothing was written."
            ),
            AssocError::SubUnread => write!(
                f,
                "couldn't read 0xE8, and the commit rewrites all three sub-window sources: \
                 writing it blind would clear them. Nothing was written."
            ),
            AssocError::NoInputList => write!(
                f,
                "the capability string's input list (60(...)) wasn't read, and a field is \
                 found by position in that list"
            ),
            AssocError::UnknownInput { input, inputs } => write!(
                f,
                "this panel doesn't list input 0x{input:02X}; it lists: {}",
                hex_list(inputs)
            ),
            AssocError::NoField { input, index } => write!(
                f,
                "input 0x{input:02X} is number {index} in this panel's input list, which \
                 under the {FIELD_ORDER} field order is outside the writable bits \
                 (0x{WRITABLE_MASK:04X}); the panel would drop the write"
            ),
            AssocError::SlotOutOfRange { slot } => {
                write!(f, "an association field is 2 bits; slot {slot} doesn't fit")
            }
        }
    }
}

/// One input's row in the association map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssocEntry {
    /// VCP 0x60 input code.
    pub input: u8,
    /// Position in the capability string's `60(...)` list.
    pub input_index: usize,
    /// Low bit of this input's field under [`FIELD_ORDER`], if it has one.
    pub shift: Option<u8>,
    /// The stored value: an index into 0xEE.
    pub slot: Option<u8>,
    /// That slot named through 0xEE, when 0xEE was read.
    pub port: Option<&'static str>,
}

impl AssocEntry {
    /// Whether this row can be written on this panel.
    pub fn writable(&self) -> bool {
        self.shift.is_some()
    }
}

/// The input -> upstream map, decoded against one panel's input list. Needs
/// 0xE7, the capability string and ideally 0xEE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Association {
    /// Names the inputs when printed.
    pub panel: &'static Panel,
    /// The raw 0xE7 word.
    pub word: u16,
    /// The raw 0xEE inventory, when read.
    pub inventory: Option<u16>,
    /// The capability string's input list, in order.
    pub inputs: Vec<u8>,
    pub entries: Vec<AssocEntry>,
}

impl Association {
    pub fn decode(
        panel: &'static Panel,
        word: u16,
        inputs: &[u8],
        inventory: Option<u16>,
    ) -> Association {
        let entries = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| {
                let shift = writable_shift_for_input(i);
                let slot = shift.map(|s| ((word >> s) & 0x3) as u8);
                AssocEntry {
                    input: *input,
                    input_index: i,
                    shift,
                    slot,
                    port: inventory.zip(slot).map(|(ee, v)| port::resolve(ee, v)),
                }
            })
            .collect();
        Association {
            panel,
            word,
            inventory,
            inputs: inputs.to_vec(),
            entries,
        }
    }

    /// Decode from a snapshot. `None` when 0xE7 wasn't read.
    pub fn from_snapshot(snap: &Snapshot) -> Option<Association> {
        let word = snap.kvm?;
        let inputs = snap.caps.legal_values(Vcp::INPUT_SOURCE).unwrap_or(&[]);
        Some(Association::decode(snap.panel, word, inputs, snap.ports))
    }

    pub fn entry(&self, input: u8) -> Option<&AssocEntry> {
        self.entries.iter().find(|e| e.input == input)
    }

    /// The upstream slot stored for an input.
    pub fn slot_of(&self, input: u8) -> Option<u8> {
        self.entry(input).and_then(|e| e.slot)
    }

    /// Re-pack the entries onto the original word. Bits no entry owns are
    /// carried through, so an unmodified map encodes to the word it came from.
    pub fn encode(&self) -> u16 {
        self.entries
            .iter()
            .filter_map(|e| e.shift.zip(e.slot))
            .fold(self.word, |w, (sh, slot)| put_field(w, sh, slot))
    }

    /// The word that stores `slot` for `input`, leaving everything else alone.
    pub fn with_input(&self, input: u8, slot: u8) -> Result<u16, AssocError> {
        if slot > 3 {
            return Err(AssocError::SlotOutOfRange { slot });
        }
        if self.inputs.is_empty() {
            return Err(AssocError::NoInputList);
        }
        let Some(entry) = self.entry(input) else {
            return Err(AssocError::UnknownInput {
                input,
                inputs: self.inputs.clone(),
            });
        };
        let Some(sh) = entry.shift else {
            return Err(AssocError::NoField {
                input,
                index: entry.input_index,
            });
        };
        Ok(put_field(self.word, sh, slot))
    }

    /// Inputs this panel lists whose field can't be written under
    /// [`FIELD_ORDER`].
    pub fn unrepresentable(&self) -> Vec<u8> {
        self.entries
            .iter()
            .filter(|e| !e.writable())
            .map(|e| e.input)
            .collect()
    }

    /// Resolve a port name (`usb-c2`) to a slot through 0xEE.
    pub fn slot_named(&self, name: &str) -> Option<u8> {
        self.inventory.and_then(|ee| port::slot_named(ee, name))
    }
}

impl fmt::Display for Association {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "0xE7 = 0x{:04X}  ({} field order)",
            self.word, FIELD_ORDER
        )?;
        for e in &self.entries {
            let name = input_name(self.panel, e.input);
            match e.shift.zip(e.slot) {
                Some((sh, slot)) => writeln!(
                    f,
                    "  {name:<12} bits {}-{sh}  slot {slot}{}",
                    sh + 1,
                    e.port.map(|p| format!(" ({p})")).unwrap_or_default()
                )?,
                None => writeln!(f, "  {name:<12} no writable field on this panel")?,
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Who owns the main window
// ---------------------------------------------------------------------------

/// Which host the main window belongs to, from 0x60's high byte (the port the
/// request arrived on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainOwner {
    /// The asking host is on screen: arrival port == selected input.
    ThisHost { input: u8 },
    /// Another machine holds the main window.
    OtherHost { arrival: u8, main: u8 },
    /// The request came over the USB-HID tunnel, whose pseudo-port can't be
    /// compared against an input.
    UsbTunnel { pseudo: u8, main: u8 },
    /// The panel didn't report an arrival port (high byte zero).
    Unreported { main: u8 },
    /// The word failed Dell's validity check; don't cache it.
    Undecodable { raw: u16 },
}

/// Decode ownership of the main window from a raw 0x60 word.
pub fn main_owner(raw: u16) -> MainOwner {
    let w = bits::input_word(raw);
    if !w.is_valid() {
        return MainOwner::Undecodable { raw };
    }
    match w.arrival {
        Arrival::Port(p) if p == w.selected => MainOwner::ThisHost { input: w.selected },
        Arrival::Port(p) => MainOwner::OtherHost {
            arrival: p,
            main: w.selected,
        },
        Arrival::UsbTunnel(p) => MainOwner::UsbTunnel {
            pseudo: p,
            main: w.selected,
        },
        Arrival::Unreported => MainOwner::Unreported { main: w.selected },
        Arrival::Invalid(_) => MainOwner::Undecodable { raw },
    }
}

impl MainOwner {
    /// `Some(false)` is a normal multi-host state, not an error.
    pub fn asker_owns_main(self) -> Option<bool> {
        match self {
            MainOwner::ThisHost { .. } => Some(true),
            MainOwner::OtherHost { .. } => Some(false),
            _ => None,
        }
    }

    /// The input the main window is showing, whoever owns it.
    pub fn main_input(self) -> Option<u8> {
        match self {
            MainOwner::ThisHost { input } => Some(input),
            MainOwner::OtherHost { main, .. }
            | MainOwner::UsbTunnel { main, .. }
            | MainOwner::Unreported { main } => Some(main),
            MainOwner::Undecodable { .. } => None,
        }
    }

    /// A short tag for `--json`.
    pub fn tag(self) -> &'static str {
        match self {
            MainOwner::ThisHost { .. } => "this-host",
            MainOwner::OtherHost { .. } => "other-host",
            MainOwner::UsbTunnel { .. } => "usb-tunnel",
            MainOwner::Unreported { .. } => "unreported",
            MainOwner::Undecodable { .. } => "undecodable",
        }
    }

    /// One line for a person, inputs named through `panel`.
    pub fn describe(self, panel: &Panel) -> String {
        let label = |code| input_label(panel, code);
        match self {
            MainOwner::ThisHost { input } => format!(
                "this host owns the main window (arrived on {}, the selected input)",
                label(input)
            ),
            MainOwner::OtherHost { arrival, main } => format!(
                "another machine owns the main window: it's showing {}, and this request \
                 arrived on {}",
                label(main),
                label(arrival)
            ),
            MainOwner::UsbTunnel { pseudo, main } => format!(
                "main window is on {}; this request came over the USB-HID tunnel \
                 (pseudo-port 0x{pseudo:02X}), so who's asking can't be told",
                label(main)
            ),
            MainOwner::Unreported { main } => format!(
                "main window is on {}; the panel didn't report an arrival port, so who's \
                 asking can't be told",
                label(main)
            ),
            MainOwner::Undecodable { raw } => {
                format!("0x60 read back 0x{raw:04X}, which isn't a valid input word")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The composite view
// ---------------------------------------------------------------------------

/// One PxP window and the upstream its source is associated with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// 0 is main; 1..=3 are 0xE8's sub-window fields.
    pub index: u8,
    /// The input filling it, when known. `Some(0)` means unassigned.
    pub input: Option<u8>,
    /// Whether the active layout has this window.
    pub live: bool,
    /// The upstream slot associated with this window's input.
    pub slot: Option<u8>,
    /// That slot named through 0xEE.
    pub port: Option<&'static str>,
}

/// Everything the KVM depends on (0x60, 0xE8, 0xE7, 0xEE, 0xE9) in one view.
/// The map is keyed by input, so 0xE7 alone can't say which window the
/// peripherals follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvmView {
    /// Names inputs and layouts when printed.
    pub panel: &'static Panel,
    pub input_word: Option<InputWord>,
    pub owner: Option<MainOwner>,
    pub layout: Option<u8>,
    pub panes: Option<Panes>,
    pub subs: Option<[u8; 3]>,
    pub assoc: Option<Association>,
    pub inventory: Option<u16>,
    pub windows: Vec<WindowInfo>,
}

/// DDC can't report which upstream is live right now.
pub const NO_LIVE_UPSTREAM_NOTE: &str =
    "0xE7 never reports which upstream is live. The map is what the panel will use, not \
     what it's using; only the host's own USB tree knows where USB is now.";

/// Build the composite view from a snapshot.
pub fn view(snap: &Snapshot) -> KvmView {
    let assoc = Association::from_snapshot(snap);
    let lookup = |input: Option<u8>| match (input, assoc.as_ref()) {
        (Some(i), Some(a)) if i != 0 => a.entry(i).map(|e| (e.slot, e.port)).unwrap_or_default(),
        _ => (None, None),
    };

    let panes = snap.panes();
    let bound = panes.map(|p| p.bound()).unwrap_or(1);
    let main = snap.main_input();
    let (slot, port) = lookup(main);
    let mut windows = vec![WindowInfo {
        index: 0,
        input: main,
        live: true,
        slot,
        port,
    }];
    if let Some(subs) = snap.sub_sources() {
        for (index, src) in (1u8..).zip(subs) {
            let (slot, port) = lookup(Some(src));
            windows.push(WindowInfo {
                index,
                input: Some(src),
                live: index < bound,
                slot,
                port,
            });
        }
    }

    KvmView {
        panel: snap.panel,
        input_word: snap.input_word(),
        owner: snap.input.map(main_owner),
        layout: snap.layout_code(),
        panes,
        subs: snap.sub_sources(),
        assoc,
        inventory: snap.ports,
        windows,
    }
}

impl fmt::Display for KvmView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.owner {
            Some(o) => writeln!(f, "  owner      {}", o.describe(self.panel))?,
            None => writeln!(f, "  owner      0x60 unreadable")?,
        }
        match self.layout {
            Some(l) => {
                let name = self.panel.value_name(Vcp::PIP_MODE, l).unwrap_or("?");
                writeln!(f, "  layout     0x{l:02X} ({name})")?
            }
            None => writeln!(f, "  layout     0xE9 unreadable")?,
        }
        for w in &self.windows {
            let what = if w.index == 0 { "main" } else { "sub" };
            let src = match w.input {
                Some(0) | None => String::from("(unassigned)"),
                Some(i) => input_label(self.panel, i),
            };
            let upstream = match (w.slot, w.port) {
                (Some(s), Some(p)) => format!("upstream slot {s} ({p})"),
                (Some(s), None) => format!("upstream slot {s}"),
                _ => String::from("no association"),
            };
            let live = if w.live {
                ""
            } else {
                "   [not in this layout]"
            };
            writeln!(f, "  w{} {what:<4} {src:<14} {upstream}{live}", w.index)?;
        }
        match &self.assoc {
            Some(a) => write!(f, "{a}")?,
            None => writeln!(f, "  0xE7       unreadable")?,
        }
        match self.inventory {
            Some(ee) => writeln!(
                f,
                "  0xEE       0x{ee:04X}  slots: {}",
                port::inventory(ee)
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{i}={}", port::nibble_name(*c)))
                    .collect::<Vec<_>>()
                    .join(" ")
            )?,
            None => writeln!(f, "  0xEE       unreadable")?,
        }
        write!(f, "  note       {NO_LIVE_UPSTREAM_NOTE}")
    }
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// Dell's KVM wizard waits a second between each commit write.
pub const KVM_DWELL: Duration = Duration::from_millis(1000);

/// Store one association map. No verify: Dell's software skips the readback
/// for this model, so a mismatch isn't treated as failure. The read after lets
/// a caller report what came back.
pub fn associate_plan(word: u16) -> Plan {
    Plan::new("kvm associate")
        .snapshotting(&[Vcp::USB_KVM])
        .write(Vcp::USB_KVM, word)
        .read(Vcp::USB_KVM)
}

/// Bind USB to a PxP window. An action: nothing to snapshot or verify.
pub fn bind_window_plan(window: u8) -> Option<Plan> {
    let value = bind_window_value(window)?;
    Some(Plan::new(format!("kvm bind window {window}")).write(Vcp::USB_KVM, value))
}

/// The 0xE7 toggle. An action, so no verify.
pub fn toggle_plan() -> Plan {
    Plan::new("kvm toggle").write(Vcp::USB_KVM, TOGGLE)
}

/// Dell's KVM wizard commit: `0x60 -> 0xE8 -> 0xE9 -> 0xE8 -> 0xE7 -> 0xE8`,
/// a second apart. The layout write re-derives the sub-sources, which is why
/// 0xE8 goes out three times. Order inferred from Dell's software.
pub fn commit_plan(main_input: u8, sub_word: u16, layout: u8, assoc: u16) -> Plan {
    Plan::new("kvm commit")
        .snapshotting(&[
            Vcp::INPUT_SOURCE,
            Vcp::PIP_SUB_SOURCE,
            Vcp::PIP_MODE,
            Vcp::USB_KVM,
        ])
        .write(Vcp::INPUT_SOURCE, main_input as u16)
        .dwell(KVM_DWELL)
        .write(Vcp::PIP_SUB_SOURCE, sub_word)
        .dwell(KVM_DWELL)
        .write(Vcp::PIP_MODE, layout as u16)
        .dwell(KVM_DWELL)
        .write(Vcp::PIP_SUB_SOURCE, sub_word)
        .dwell(KVM_DWELL)
        .write(Vcp::USB_KVM, assoc)
        .dwell(KVM_DWELL)
        .write(Vcp::PIP_SUB_SOURCE, sub_word)
}

/// Everything the wizard commit writes, resolved before any traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    pub main_input: u8,
    pub sub_word: u16,
    pub layout: u8,
    pub assoc: u16,
}

impl Commit {
    /// Resolve a commit against the current state. `sub1` and `associate` are
    /// optional; leaving one out keeps what the panel holds. The commit rewrites
    /// 0xE8 and 0xE7 either way, so both must have been read.
    pub fn build(
        snap: &Snapshot,
        main_input: u8,
        sub1: Option<u8>,
        layout: u8,
        associate: Option<(u8, u8)>,
    ) -> Result<Commit, AssocError> {
        let current_sub = snap.sub.ok_or(AssocError::SubUnread)?;
        let map = Association::from_snapshot(snap).ok_or(AssocError::MapUnread)?;
        let sub_word = sub1.map_or(current_sub, |i| pip_sub::with_sub1(current_sub, i));
        let assoc = match associate {
            Some((input, slot)) => map.with_input(input, slot)?,
            None => map.word,
        };
        Ok(Commit {
            main_input,
            sub_word,
            layout,
            assoc,
        })
    }

    pub fn plan(&self) -> Plan {
        commit_plan(self.main_input, self.sub_word, self.layout, self.assoc)
    }
}

// ---------------------------------------------------------------------------
// Checks
//
// These add what `guard::evaluate` can't see. They don't repeat the evaluate
// of the intent passed to `Runner::apply`, which the runner does itself.
// ---------------------------------------------------------------------------

/// What's wrong with storing `slot` for `input`, most severe first. Goes with
/// [`Intent::KvmAssociate`].
pub fn check_associate(snap: &Snapshot, input: u8, slot: u8) -> Vec<Verdict> {
    let Some(assoc) = Association::from_snapshot(snap) else {
        return vec![Verdict::Refuse(AssocError::MapUnread.to_string())];
    };
    let mut out = Vec::new();
    match assoc.with_input(input, slot) {
        Ok(word) if word == assoc.word => out.push(Verdict::Warn(format!(
            "{} is already on slot {slot}; nothing to change",
            input_label(snap.panel, input)
        ))),
        Ok(_) => {}
        Err(e) => out.push(Verdict::Refuse(e.to_string())),
    }

    // Both orders agree at one index, so only warn where they don't.
    let index = assoc.entry(input).map(|e| e.input_index);
    let orders_differ = index.is_some_and(|i| {
        FieldOrder::LowFirst.shift_for_input(i) != FieldOrder::DdpmHighFirst.shift_for_input(i)
    });
    if orders_differ {
        let dell = match ddpm_order(&snap.caps) {
            Some(theirs) if theirs != FIELD_ORDER => {
                format!(" Dell's software would pack it {theirs} on this panel.")
            }
            _ => String::new(),
        };
        out.push(Verdict::Warn(format!(
            "0xE7's field order is unresolved; this uses {FIELD_ORDER}.{dell} If USB lands on \
             the wrong host, flip ddc_core::kvm::FIELD_ORDER."
        )));
    }
    guard::tidy(out)
}

/// Whether the per-window USB bind can be sent. Goes with
/// `Intent::Raw { vcp: 0xE7, .. }`.
pub fn check_bind_window(snap: &Snapshot, window: u8) -> Vec<Verdict> {
    let Some(value) = bind_window_value(window) else {
        return vec![Verdict::Refuse(format!(
            "0xE7 binds PxP windows 1..={MAX_BIND_WINDOW} (0xFF01..=0xFF04); there's no \
             window {window}"
        ))];
    };
    let mut out = Vec::new();
    if window_bind_advertised(&snap.caps) {
        out.push(Verdict::Warn(String::from(
            "0xFF01..=0xFF04 comes from Dell's software and is untested; it's an action with \
             no readback",
        )));
    } else {
        let listed = match snap.caps.legal_values(Vcp::USB_KVM) {
            Some(v) if !v.is_empty() => hex_list(v),
            _ => String::from("(no value list)"),
        };
        out.push(Verdict::Refuse(format!(
            "this panel doesn't offer the per-window USB bind: Dell's software only shows it \
             when 'FF' is in the E7 value list, and this panel advertises E7({listed}). \
             0x{value:04X} is write-only with no readback, so nothing was written. Use \
             `kvm switch` (0xFF00) instead."
        )));
    }
    // Bind windows are 1-based; pane indices are 0-based.
    match snap.layout_code() {
        Some(l) if !bits::panes(l).has_window(window - 1) => out.push(Verdict::Refuse(format!(
            "the active layout 0x{l:02X} has {} window(s); there's no window {window} to \
             bind USB to",
            bits::panes(l).bound()
        ))),
        None => out.push(Verdict::Warn(String::from(
            "couldn't read the layout (0xE9), so the window number wasn't checked",
        ))),
        _ => {}
    }
    guard::tidy(out)
}

/// Checks for the wizard commit's other writes. Goes with
/// `Intent::SetInput { input: c.main_input }`.
pub fn check_commit(snap: &Snapshot, c: &Commit) -> Vec<Verdict> {
    let mut out = guard::evaluate(snap, &Intent::SetLayout { layout: c.layout });
    let sub1 = pip_sub::sub1_of(c.sub_word);
    if sub1 != 0 && Some(c.sub_word) != snap.sub {
        out.extend(guard::evaluate(
            snap,
            &Intent::SetSubSource {
                window: 1,
                input: sub1,
            },
        ));
    }
    if let Some(current) = snap.kvm.filter(|w| *w != c.assoc) {
        for (new, old) in decode(c.assoc).into_iter().zip(decode(current)) {
            if new != old {
                out.extend(guard::evaluate(snap, &Intent::KvmAssociate { slot: new }));
            }
        }
    }
    if sub1 != 0 && c.main_input == sub1 {
        out.push(guard::mirrored(snap, sub1, "in sub-window 1"));
    }
    guard::tidy(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::idle_snapshot as live;
    use crate::guard::{refusals_of, warnings_of};
    use crate::vcp::{default_panel, input};

    /// The order this panel's capability string lists its inputs in.
    const INPUTS: [u8; 5] = [0x1B, 0x0F, 0x13, 0x11, 0x12];

    // ---- bit layout ------------------------------------------------------

    #[test]
    fn decodes_observed_baseline() {
        assert_eq!(decode(0x2540), [1, 1, 1, 2]);
        // 0xEE = 0xBA98: slots are usb-c1..c4, no USB-B.
        assert_eq!(port::inventory(0xBA98), [0x8, 0x9, 0xA, 0xB]);
        assert_eq!(port::resolve(0xBA98, 1), "usb-c2");
        assert_eq!(port::slot_named(0xBA98, "usb-c3"), Some(2));
        assert_eq!(port::slot_named(0xBA98, "usb-b1"), None);
    }

    #[test]
    fn the_two_field_orders_agree_at_exactly_one_index() {
        let agree: Vec<usize> = (0..5)
            .filter(|k| {
                FieldOrder::LowFirst.shift_for_input(*k)
                    == FieldOrder::DdpmHighFirst.shift_for_input(*k)
            })
            .collect();
        assert_eq!(agree, vec![2]);
        assert_eq!(FieldOrder::LowFirst.shift_for_input(2), Some(10));
    }

    #[test]
    fn flipping_the_order_moves_every_other_input_and_not_dp2() {
        let low = Association::decode(default_panel(), 0x2540, &INPUTS, Some(0xBA98));
        assert_eq!(low.slot_of(0x13), Some(1));
        // Dell's reading of the same word: index k at bits 14-2k.
        let ddpm: Vec<u8> = (0..5)
            .map(|k| ((0x2540u16 >> (14 - 2 * k)) & 3) as u8)
            .collect();
        assert_eq!(ddpm, vec![0, 2, 1, 1, 1]);
        assert_eq!(ddpm[2], low.slot_of(0x13).unwrap());
        assert_ne!(ddpm[0], low.slot_of(0x1B).unwrap());
    }

    #[test]
    fn only_writable_shifts_are_offered() {
        assert!(shift_is_writable(6));
        assert!(shift_is_writable(12));
        assert!(!shift_is_writable(14));
        assert!(!shift_is_writable(4));
        for k in 0..8 {
            if let Some(s) = writable_shift_for_input(k) {
                assert!(
                    FIELD_SHIFTS.contains(&s),
                    "shift {s} is not a writable field"
                );
            }
        }
    }

    // ---- the map as a value ---------------------------------------------

    #[test]
    fn association_round_trips_the_observed_word() {
        let a = Association::decode(default_panel(), 0x2540, &INPUTS, Some(0xBA98));
        assert_eq!(a.encode(), 0x2540);
        assert_eq!(a.entries.len(), 5);
        assert_eq!(a.slot_of(0x1B), Some(1));
        assert!(!a.entry(0x12).unwrap().writable());
        assert_eq!(a.unrepresentable(), vec![0x12]);
        assert_eq!(a.entry(0x13).unwrap().port, Some("usb-c2"));
    }

    #[test]
    fn association_round_trips_every_writable_slot() {
        let a = Association::decode(default_panel(), 0x2540, &INPUTS, Some(0xBA98));
        for input in [0x1Bu8, 0x0F, 0x13, 0x11] {
            for slot in 0..4u8 {
                let word = a.with_input(input, slot).unwrap();
                let back = Association::decode(default_panel(), word, &INPUTS, Some(0xBA98));
                assert_eq!(back.slot_of(input), Some(slot));
                assert_eq!(back.encode(), word);
                assert_eq!(word & !WRITABLE_MASK, 0x2540 & !WRITABLE_MASK);
                for other in [0x1Bu8, 0x0F, 0x13, 0x11]
                    .into_iter()
                    .filter(|o| *o != input)
                {
                    assert_eq!(back.slot_of(other), a.slot_of(other));
                }
            }
        }
    }

    #[test]
    fn association_refuses_what_it_cannot_encode() {
        let a = Association::decode(default_panel(), 0x2540, &INPUTS, Some(0xBA98));
        assert_eq!(
            a.with_input(0x12, 1),
            Err(AssocError::NoField {
                input: 0x12,
                index: 4
            })
        );
        assert!(matches!(
            a.with_input(0x99, 1),
            Err(AssocError::UnknownInput { .. })
        ));
        assert_eq!(
            a.with_input(0x13, 9),
            Err(AssocError::SlotOutOfRange { slot: 9 })
        );
        let blind = Association::decode(default_panel(), 0x2540, &[], None);
        assert_eq!(blind.with_input(0x13, 1), Err(AssocError::NoInputList));
    }

    #[test]
    fn association_comes_out_of_a_snapshot() {
        let a = Association::from_snapshot(&live()).unwrap();
        assert_eq!(a.word, 0x2540);
        assert_eq!(a.inventory, Some(0xBA98));
        assert_eq!(a.inputs, INPUTS.to_vec());
        assert_eq!(a.slot_named("usb-c3"), Some(2));
    }

    // ---- who owns the main window ---------------------------------------

    #[test]
    fn identifies_the_host_that_owns_the_main_window() {
        let o = main_owner(0x1B1B);
        assert_eq!(
            o,
            MainOwner::ThisHost {
                input: input::USB_C
            }
        );
        assert_eq!(o.asker_owns_main(), Some(true));
        assert_eq!(o.tag(), "this-host");

        // Arrived on DP2 while another machine held main on DP1.
        let o = main_owner(0x130F);
        assert_eq!(
            o,
            MainOwner::OtherHost {
                arrival: input::DP2,
                main: input::DISPLAY_PORT
            }
        );
        assert_eq!(o.asker_owns_main(), Some(false));
        assert_eq!(o.main_input(), Some(input::DISPLAY_PORT));
        assert!(o.describe(default_panel()).contains("another machine"));
    }

    #[test]
    fn a_usb_tunnel_arrival_is_not_an_ownership_claim() {
        let o = main_owner(0x8313);
        assert_eq!(
            o,
            MainOwner::UsbTunnel {
                pseudo: 0x83,
                main: input::DP2
            }
        );
        assert_eq!(o.asker_owns_main(), None);
        assert_eq!(main_owner(0x001B).asker_owns_main(), None);
        assert_eq!(main_owner(0x1B40).tag(), "undecodable");
    }

    // ---- the composite view ---------------------------------------------

    #[test]
    fn view_joins_every_register_the_kvm_depends_on() {
        let v = view(&live());
        assert_eq!(v.layout, Some(0x24));
        assert_eq!(v.owner.unwrap().asker_owns_main(), Some(true));
        assert_eq!(
            v.subs,
            Some([input::HDMI_1, input::DISPLAY_PORT, input::USB_C])
        );
        // 2-up: main plus one live sub, and two sub fields the layout can't show.
        assert_eq!(v.windows.len(), 4);
        assert_eq!(v.windows.iter().filter(|w| w.live).count(), 2);
        let main = &v.windows[0];
        assert_eq!(main.input, Some(input::USB_C));
        assert_eq!(main.slot, Some(1));
        assert_eq!(main.port, Some("usb-c2"));
        let sub = &v.windows[1];
        assert_eq!(sub.input, Some(input::HDMI_1));
        assert_eq!(sub.slot, Some(2));
        assert!(sub.live);
        assert!(!v.windows[2].live);
        let text = v.to_string();
        assert!(text.contains("usb-c2"));
        assert!(text.contains("never reports which upstream is live"));
    }

    #[test]
    fn view_survives_a_panel_that_answered_nothing() {
        let v = view(&Snapshot::new(default_panel(), Capabilities::default()));
        assert!(v.assoc.is_none());
        assert_eq!(v.windows.len(), 1);
        assert!(v.to_string().contains("unreadable"));
    }

    // ---- checks ----------------------------------------------------------

    #[test]
    fn associating_an_unreadable_map_is_refused_not_guessed() {
        let mut s = live();
        s.kvm = None;
        let r = refusals_of(&check_associate(&s, 0x13, 2));
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("read-modify-write"), "{r:?}");
    }

    #[test]
    fn the_field_order_warning_appears_only_where_the_orders_disagree() {
        // dp1 is index 1: the two orders put it in different bits.
        let w = warnings_of(&check_associate(&live(), 0x0F, 2));
        assert_eq!(
            w.iter()
                .filter(|m| m.contains("field order is unresolved"))
                .count(),
            1
        );
        // This panel contains "EE", so Dell's software would disagree.
        assert!(w[0].contains("Dell's software would pack it"), "{w:?}");
        // dp2 is index 2, where both orders agree.
        assert!(warnings_of(&check_associate(&live(), 0x13, 2)).is_empty());
        let plain = Capabilities::parse("(vcp(60( 1B 0F) E7(00 01) F1 F2))");
        assert_eq!(ddpm_order(&plain), None);
    }

    #[test]
    fn associating_an_input_with_no_field_is_refused() {
        let r = refusals_of(&check_associate(&live(), 0x12, 1));
        assert!(
            r.iter().any(|m| m.contains("outside the writable bits")),
            "{r:?}"
        );
    }

    #[test]
    fn an_oversized_slot_is_refused_once_across_guard_and_check() {
        let s = live();
        let mut v = guard::evaluate(&s, &Intent::KvmAssociate { slot: 4 });
        v.extend(check_associate(&s, 0x13, 4));
        assert_eq!(refusals_of(&guard::tidy(v)).len(), 1);
    }

    #[test]
    fn associating_the_value_already_stored_says_so() {
        // dp2 already holds slot 1 in 0x2540.
        let v = check_associate(&live(), 0x13, 1);
        assert!(
            warnings_of(&v)
                .iter()
                .any(|m| m.contains("nothing to change")),
            "{v:?}"
        );
        assert!(refusals_of(&v).is_empty());
    }

    #[test]
    fn window_bind_is_refused_on_a_panel_that_does_not_advertise_ff() {
        let r = refusals_of(&check_bind_window(&live(), 1));
        assert!(
            r.iter()
                .any(|m| m.contains("'FF'") && m.contains("E7(00 01 02 03)")),
            "{r:?}"
        );
        assert!(r.iter().any(|m| m.contains("nothing was written")), "{r:?}");
        assert!(!window_bind_advertised(&live().caps));
    }

    #[test]
    fn window_bind_is_allowed_but_flagged_where_ff_is_advertised() {
        let caps = Capabilities::parse(
            "(vcp(60( 1B 0F 13 11 12) E7(00 01 02 03 FF) E8 E9(00 24) EE F1 F2))",
        );
        let mut s = Snapshot::new(default_panel(), caps);
        s.status = Some(0);
        s.layout = Some(0x24);
        let v = check_bind_window(&s, 2);
        assert!(refusals_of(&v).is_empty(), "{v:?}");
        assert!(warnings_of(&v).iter().any(|m| m.contains("untested")));
        assert!(refusals_of(&check_bind_window(&s, 3))
            .iter()
            .any(|m| m.contains("window(s)")));
        assert!(refusals_of(&check_bind_window(&s, 5))[0].contains("no window 5"));
    }

    #[test]
    fn bind_window_values_match_the_documented_frames() {
        assert_eq!(bind_window_value(1), Some(0xFF01));
        assert_eq!(bind_window_value(4), Some(0xFF04));
        assert_eq!(bind_window_value(0), None);
        assert_eq!(bind_window_value(5), None);
    }

    // ---- the commit -------------------------------------------------------

    #[test]
    fn commit_resolves_words_from_the_current_state() {
        let s = live();
        let c = Commit::build(&s, input::DP2, Some(input::HDMI_1), 0x24, Some((0x0F, 3))).unwrap();
        assert_eq!(c.main_input, input::DP2);
        assert_eq!(c.sub_word, 0x6DF1);
        assert_eq!(c.layout, 0x24);
        assert_eq!(c.assoc, 0x2540 | (0x3 << 8)); // dp1 -> slot 3
        assert_eq!(
            Association::decode(default_panel(), c.assoc, &INPUTS, None).slot_of(0x0F),
            Some(3)
        );
    }

    #[test]
    fn commit_keeps_what_the_caller_did_not_ask_to_change() {
        let s = live();
        let c = Commit::build(&s, input::DP2, None, 0x24, None).unwrap();
        assert_eq!(c.sub_word, s.sub.unwrap());
        assert_eq!(c.assoc, s.kvm.unwrap());
    }

    #[test]
    fn commit_refuses_to_write_a_map_or_sub_word_it_never_read() {
        // Both are always rewritten, so a blind 0x0000 would wipe them.
        let mut s = live();
        s.kvm = None;
        assert_eq!(
            Commit::build(&s, input::DP2, None, 0x24, None),
            Err(AssocError::MapUnread)
        );
        assert_eq!(
            Commit::build(&s, input::DP2, None, 0x24, Some((0x0F, 3))),
            Err(AssocError::MapUnread)
        );
        let mut s = live();
        s.sub = None;
        assert_eq!(
            Commit::build(&s, input::DP2, None, 0x24, None),
            Err(AssocError::SubUnread)
        );
        assert!(AssocError::MapUnread.to_string().contains("0xE7"));
        assert!(AssocError::SubUnread.to_string().contains("0xE8"));
    }

    #[test]
    fn commit_plan_is_ddpms_six_writes_in_order() {
        let c = Commit {
            main_input: input::DP2,
            sub_word: 0x6DF1,
            layout: 0x24,
            assoc: 0x2540,
        };
        let p = c.plan();
        assert_eq!(
            p.writes(),
            vec![
                (0x60, 0x0013),
                (0xE8, 0x6DF1),
                (0xE9, 0x0024),
                (0xE8, 0x6DF1),
                (0xE7, 0x2540),
                (0xE8, 0x6DF1),
            ]
        );
        for code in p.written_codes() {
            assert!(p.snapshot.contains(&code));
        }
        // A dwell between every pair of writes.
        assert_eq!(p.total_dwell(), KVM_DWELL * 5);
        for pair in p.steps.windows(2) {
            assert!(!(pair[0].is_write() && pair[1].is_write()));
        }
    }

    #[test]
    fn commit_undo_restores_the_layout_before_the_sub_sources() {
        let c = Commit {
            main_input: input::DP2,
            sub_word: 0x6DF1,
            layout: 0x41,
            assoc: 0x2540,
        };
        let undo = c
            .plan()
            .undo_from(&[
                (0x60, 0x1B1B),
                (0xE8, 0x6DF3),
                (0xE9, 0x0024),
                (0xE7, 0x2540),
            ])
            .unwrap();
        let order: Vec<u8> = undo.writes().iter().map(|(c, _)| *c).collect();
        assert_eq!(order, vec![0xE9, 0x60, 0xE8, 0xE7]);
    }

    #[test]
    fn commit_allows_main_and_sub_on_one_source_and_says_it_mirrors() {
        let c = Commit {
            main_input: input::HDMI_1,
            sub_word: 0x6DF1,
            layout: 0x24,
            assoc: 0x2540,
        };
        let v = check_commit(&live(), &c);
        assert!(refusals_of(&v).is_empty(), "{v:?}");
        assert!(
            warnings_of(&v).iter().any(|m| m.contains("appear twice")),
            "{v:?}"
        );
    }

    #[test]
    fn commit_refuses_a_layout_the_panel_does_not_list() {
        let c = Commit {
            main_input: input::DP2,
            sub_word: 0x6DF1,
            layout: 0x42,
            assoc: 0x2540,
        };
        assert!(refusals_of(&check_commit(&live(), &c))
            .iter()
            .any(|m| m.contains("0xE9")));
    }

    // ---- plans ------------------------------------------------------------

    #[test]
    fn associate_plan_writes_then_reads_but_never_verifies() {
        let p = associate_plan(0x2440);
        assert_eq!(p.writes(), vec![(0xE7, 0x2440)]);
        assert_eq!(p.snapshot, vec![0xE7]);
        assert!(!p
            .steps
            .iter()
            .any(|s| matches!(s, crate::plan::Step::Verify { .. })));
        assert_eq!(
            p.undo_from(&[(0xE7, 0x2540)]).unwrap().writes(),
            vec![(0xE7, 0x2540)]
        );
    }

    #[test]
    fn action_plans_have_nothing_to_undo() {
        let p = bind_window_plan(2).unwrap();
        assert_eq!(p.writes(), vec![(0xE7, 0xFF02)]);
        assert!(p.snapshot.is_empty());
        assert!(p.undo_from(&[]).is_none());
        assert!(bind_window_plan(9).is_none());

        let t = toggle_plan();
        assert_eq!(t.writes(), vec![(0xE7, TOGGLE)]);
        assert!(!t
            .steps
            .iter()
            .any(|s| matches!(s, crate::plan::Step::Verify { .. })));
    }
}
