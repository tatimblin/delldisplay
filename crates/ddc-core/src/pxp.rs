//! PiP/PBP: windows, plans, checks and layout geometry.
//!
//! * [`Window`]: `main`, `sub1`, `sub2`, `sub3`. The same indices name 0xE5's
//!   swap nibbles and 0xE8's three fields.
//! * Plans: [`swap_plan`] and [`zoom_plan`]/[`underscan_plan`] (all 0xE5, an
//!   overloaded action register), [`cycle_inset_plan`] (0xE9 = 0x02),
//!   [`sub_source_plan`] (0xE8) and [`apply_plan`] (layout, input and
//!   sub-sources in one go).
//! * 0xE8 helpers: [`sub_sources`], [`with_sub`], [`apply_subs`].
//! * The PiP inset corner, which isn't readable: [`InsetTracker`].
//! * Checks: [`check_swap`], [`check_inset_cycle`], [`check_apply`].
//! * Geometry: [`geometry`] and [`all_layouts`], from a table with no traffic.
//!
//! Only `0xF010` (swap main and sub1) has been seen working on a panel; the
//! other 0xE5 words follow the same encoding but are inferred. 0xE5 is
//! double-written like everything else.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::bits::{self, Panes};
use crate::caps::Capabilities;
use crate::guard::{self, Intent, Snapshot, Verdict};
use crate::plan::{Plan, LAYOUT_SETTLE};
use crate::vcp::{pip, pip_sub, Panel, Provenance, Vcp};

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// A PxP window, indexed the way 0xE5 and 0xE8 both index them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Window {
    /// The main window. Its source is VCP 0x60, never 0xE8.
    Main = 0,
    /// 0xE8 bits 0-4.
    Sub1 = 1,
    /// 0xE8 bits 5-9.
    Sub2 = 2,
    /// 0xE8 bits 10-14.
    Sub3 = 3,
}

impl Window {
    pub const ALL: [Window; 4] = [Window::Main, Window::Sub1, Window::Sub2, Window::Sub3];

    pub fn index(self) -> u8 {
        self as u8
    }

    pub fn from_index(i: u8) -> Option<Window> {
        Window::ALL.get(i as usize).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Window::Main => "main",
            Window::Sub1 => "sub1",
            Window::Sub2 => "sub2",
            Window::Sub3 => "sub3",
        }
    }

    pub fn is_main(self) -> bool {
        self == Window::Main
    }

    /// Which 0xE8 field carries this window's source. `None` for main.
    pub fn sub_field(self) -> Option<usize> {
        match self {
            Window::Main => None,
            w => Some(w.index() as usize - 1),
        }
    }
}

/// Accepts `main`/`sub1`..`sub3`, `m`/`s1`..`s3`, or a bare window index, so
/// `1` is sub1 and `0` is main.
impl FromStr for Window {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "main" | "m" | "0" => Ok(Window::Main),
            "sub1" | "s1" | "1" => Ok(Window::Sub1),
            "sub2" | "s2" | "2" => Ok(Window::Sub2),
            "sub3" | "s3" | "3" => Ok(Window::Sub3),
            _ => Err(format!("unknown window '{s}' (main|sub1|sub2|sub3)")),
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// 0xE5 and plans
// ---------------------------------------------------------------------------

/// The idle word Dell's software writes after a swap.
pub const SWAP_RELEASE: u16 = 0xF000;
/// The one 0xE5 word seen working on a panel: swap main and sub1.
pub const SWAP_MAIN_SUB1: u16 = 0xF010;
/// 0xE5: step the PBP zoom.
const ZOOM_STEP: u16 = 0x0002;
/// 0xE5: toggle underscan.
const UNDERSCAN_STEP: u16 = 0x0003;
/// Wait before the release word. Dell's software sleeps here; the length is a
/// guess.
const SWAP_RELEASE_DELAY: Duration = Duration::from_millis(500);

/// Written to 0xE9 to step the PiP inset one corner.
pub const INSET_STEP: u16 = 0x0002;

/// `0xF000 | hi << 4 | lo`, larger index in the high nibble so main/sub1 comes
/// out as the verified `0xF010`.
pub fn swap_word(a: Window, b: Window) -> u16 {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    SWAP_RELEASE | ((hi.index() as u16) << 4) | (lo.index() as u16)
}

/// Swap two windows. No verify and no snapshot: 0xE5 doesn't read back.
pub fn swap_plan(a: Window, b: Window) -> Plan {
    Plan::new(format!("pxp swap {a}<->{b}")).write(Vcp::PXP_ACTION, swap_word(a, b))
}

/// [`swap_plan`] plus Dell's delayed release write. Not the default: the
/// verified swap was a bare `0xF010`.
pub fn swap_plan_with_release(a: Window, b: Window) -> Plan {
    Plan::new(format!("pxp swap {a}<->{b} with release"))
        .write(Vcp::PXP_ACTION, swap_word(a, b))
        .dwell(SWAP_RELEASE_DELAY)
        .write(Vcp::PXP_ACTION, SWAP_RELEASE)
}

/// Step the PBP zoom (0xE5 = 0x0002).
pub fn zoom_plan() -> Plan {
    Plan::new("pbp zoom step").write(Vcp::PXP_ACTION, ZOOM_STEP)
}

/// Toggle underscan (0xE5 = 0x0003).
pub fn underscan_plan() -> Plan {
    Plan::new("underscan toggle").write(Vcp::PXP_ACTION, UNDERSCAN_STEP)
}

/// Step the PiP inset one corner (0xE9 = 0x02). Only valid in 0x21/0x22; see
/// [`check_inset_cycle`]. No undo: the corner can't be read.
pub fn cycle_inset_plan() -> Plan {
    Plan::new("pip inset step").write(Vcp::PIP_MODE, INSET_STEP)
}

/// Point one sub-window at `input`, keeping the other two fields. `None` for
/// main. Verified, because a refused 0xE8 write otherwise looks like success.
pub fn sub_source_plan(current: u16, window: Window, input: u8) -> Option<Plan> {
    let word = with_sub(current, window, input)?;
    Some(
        Plan::new(format!("pxp {window} source"))
            // 0xE9 is snapshotted for context; undo only restores what was written.
            .snapshotting(&[Vcp::PIP_MODE, Vcp::PIP_SUB_SOURCE])
            .write(Vcp::PIP_SUB_SOURCE, word)
            .verify(Vcp::PIP_SUB_SOURCE, word),
    )
}

/// Apply a layout, main input and sub-sources in one go. The layout goes first
/// because it decides which 0xE8 fields are live; each layout or input write
/// waits for the panel to answer.
pub fn apply_plan(layout: u8, main_input: u8, current_sub: u16, changes: &[(Window, u8)]) -> Plan {
    Plan::new("pxp apply")
        .snapshotting(&[Vcp::PIP_MODE, Vcp::INPUT_SOURCE, Vcp::PIP_SUB_SOURCE])
        .write(Vcp::PIP_MODE, layout as u16)
        .await_ready(Vcp::PIP_MODE, LAYOUT_SETTLE)
        .write(Vcp::INPUT_SOURCE, main_input as u16)
        .await_ready(Vcp::INPUT_SOURCE, LAYOUT_SETTLE)
        .write(Vcp::PIP_SUB_SOURCE, apply_subs(current_sub, changes))
}

// ---------------------------------------------------------------------------
// 0xE8
// ---------------------------------------------------------------------------

/// The three sub-window sources, sub1 first.
pub fn sub_sources(word: u16) -> [u8; 3] {
    pip_sub::decode(word)
}

/// Replace one sub-window's source, keeping the other two. `None` for main.
/// The input is masked to the 5-bit field width.
pub fn with_sub(current: u16, window: Window, input: u8) -> Option<u16> {
    let field = window.sub_field()?;
    let mut subs = sub_sources(current);
    subs[field] = input & 0x1F;
    Some(pip_sub::encode(subs))
}

/// Apply several assignments in order. Entries for main are skipped.
pub fn apply_subs(current: u16, changes: &[(Window, u8)]) -> u16 {
    changes.iter().fold(current, |w, (win, input)| {
        with_sub(w, *win, *input).unwrap_or(w)
    })
}

// ---------------------------------------------------------------------------
// The PiP inset corner
// ---------------------------------------------------------------------------

/// Dell's corner names in cycle order. That the panel steps in this order
/// hasn't been checked.
pub const INSET_CORNERS: [&str; 4] = ["right-top", "left-top", "right-bottom", "left-bottom"];

/// Why the inset corner is never reported as fact.
pub const CORNER_NOT_READABLE: &str =
    "the PiP inset corner isn't readable over DDC; this is a step count from an assumed \
     starting corner, and the monitor's own menu can move it";

/// A guess that carries its caveat. Display prints both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unverified {
    value: &'static str,
    because: &'static str,
}

impl Unverified {
    pub fn guess(self) -> &'static str {
        self.value
    }
    pub fn because(self) -> &'static str {
        self.because
    }
}

impl fmt::Display for Unverified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (unverified: {})", self.value, self.because)
    }
}

/// A host-side count of inset corner steps. The count is exact; the corner it
/// implies isn't. The default assumes the inset starts at `right-top`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InsetTracker {
    steps: u32,
    start: u8,
}

impl InsetTracker {
    /// Seed with a starting corner index and step count.
    pub fn resumed(start: u8, steps: u32) -> Self {
        InsetTracker {
            steps,
            start: start % 4,
        }
    }

    /// Record one `0xE9 = 0x02` write.
    pub fn step(&mut self) {
        self.steps = self.steps.wrapping_add(1);
    }

    pub fn steps(self) -> u32 {
        self.steps
    }

    /// Where the inset probably is now.
    pub fn corner(self) -> Unverified {
        let i = (self.start as u32 + self.steps) % 4;
        Unverified {
            value: INSET_CORNERS[i as usize],
            because: CORNER_NOT_READABLE,
        }
    }
}

/// Whether this layout has an inset to move (0x21 and 0x22 only).
pub fn layout_has_inset(layout: u8) -> bool {
    matches!(layout, pip::PIP_SMALL | pip::PIP_LARGE)
}

// ---------------------------------------------------------------------------
// Checks
//
// These add what `guard::evaluate` can't see. They don't repeat the evaluate
// of the intent passed to `Runner::apply`, which the runner does itself.
// ---------------------------------------------------------------------------

/// Checks for stepping the inset. Goes with `Intent::Raw { vcp: 0xE9, .. }`.
pub fn check_inset_cycle(snap: &Snapshot) -> Vec<Verdict> {
    let mut out = Vec::new();
    match snap.layout_code() {
        Some(l) if layout_has_inset(l) => {}
        Some(l) => out.push(Verdict::Refuse(format!(
            "the inset only exists in picture-in-picture, and the active layout is \
             0x{l:02X}{}. Set `pip` or `pip-large` first. Nothing was written.",
            layout_label(snap.panel, l)
        ))),
        None => out.push(Verdict::Warn(String::from(
            "couldn't read the layout (0xE9), so it's unknown whether there's an inset to \
             move; the panel ignores the write if there isn't",
        ))),
    }
    // Dell's software only offers the step when 0x02 is in 0xE9's list.
    if let Some(values) = snap.caps.legal_values(Vcp::PIP_MODE) {
        if !values.is_empty() && !values.contains(&(INSET_STEP as u8)) {
            out.push(Verdict::Refuse(String::from(
                "this panel doesn't advertise 0x02 in its 0xE9 value list, which is how Dell's \
                 software decides the inset step exists",
            )));
        }
    }
    guard::tidy(out)
}

/// Dell's software only offers a swap in a two-window layout: `0xE9 != 0` and
/// (`<= 0x2F` or `== 0x51`). A UI gate, not a panel refusal.
pub fn ddpm_offers_swap(layout: u8) -> bool {
    layout != pip::OFF && (layout <= 0x2F || layout == 0x51)
}

/// Checks for a swap. Goes with [`Intent::PxpSwap`].
pub fn check_swap(snap: &Snapshot) -> Vec<Verdict> {
    match snap.layout_code() {
        Some(l) if !ddpm_offers_swap(l) => vec![Verdict::Warn(format!(
            "Dell's software doesn't offer a swap in layout 0x{l:02X} (only 0x01..0x2F and \
             0x51). Sending it anyway; the panel may ignore it."
        ))],
        _ => Vec::new(),
    }
}

/// Checks for [`apply_plan`], judged against the layout being moved to. Goes
/// with `Intent::SetLayout { layout }`.
///
/// The input and sub-source intents are evaluated against a projected
/// snapshot with 0xE9, 0x60 and 0xE8 already set to what the plan writes, since
/// the pane count that matters is the one after the layout lands.
pub fn check_apply(
    snap: &Snapshot,
    layout: u8,
    main_input: u8,
    changes: &[(Window, u8)],
) -> Vec<Verdict> {
    let mut out = Vec::new();

    // From PiP, 0x01/0x02 are step actions rather than layouts.
    let canonical = match snap.layout_code().map(|now| pip::write_effect(now, layout)) {
        Some(pip::Effect::Layout(l)) => l,
        Some(step) => {
            let what = match step {
                pip::Effect::StepInsetCorner => "corner-step",
                _ => "size-step",
            };
            out.push(Verdict::Refuse(format!(
                "0x{layout:02X} isn't a layout while the panel is in PiP; it's a {what} \
                 action there. Use `pxp inset` for the corner, or write the layout you want."
            )));
            pip::canonical(layout)
        }
        None => pip::canonical(layout),
    };

    if changes.iter().any(|(w, _)| w.is_main()) {
        out.push(Verdict::Refuse(String::from(
            "the main window's source is VCP 0x60, not a 0xE8 field; pass it as the main input",
        )));
    }

    let after = apply_subs(snap.sub.unwrap_or(0), changes);

    // The main input, against the sub-sources the apply ends with.
    let mut with_final_subs = snap.clone();
    with_final_subs.put(Vcp::PIP_MODE, canonical as u16);
    with_final_subs.put(Vcp::PIP_SUB_SOURCE, after);
    out.extend(guard::evaluate(
        &with_final_subs,
        &Intent::SetInput { input: main_input },
    ));

    // Each sub-window, against the layout and main input the apply ends with.
    let mut with_final_main = snap.clone();
    with_final_main.put(Vcp::PIP_MODE, canonical as u16);
    with_final_main.put(Vcp::INPUT_SOURCE, main_input as u16);
    for (w, input) in changes.iter().filter(|(w, _)| !w.is_main()) {
        out.extend(guard::evaluate(
            &with_final_main,
            &Intent::SetSubSource {
                window: w.index(),
                input: *input,
            },
        ));
    }
    guard::tidy(out)
}

fn layout_label(panel: &Panel, layout: u8) -> String {
    panel
        .value_name(Vcp::PIP_MODE, layout)
        .map(|n| format!(" ({n})"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// One window's place in a layout's grid. Grid units rather than pixels: a
/// caller that wants pixels knows the panel's resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// 0 main, 1 sub1, 2 sub2, 3 sub3.
    pub window: u8,
    pub col: u8,
    pub row: u8,
    pub col_span: u8,
    pub row_span: u8,
    /// The PiP inset, drawn over main. Its corner isn't readable, so its
    /// position and spans are all zero.
    pub overlay: bool,
}

const fn cell(window: u8, col: u8, row: u8, col_span: u8, row_span: u8) -> Cell {
    Cell {
        window,
        col,
        row,
        col_span,
        row_span,
        overlay: false,
    }
}

const fn inset(window: u8) -> Cell {
    Cell {
        window,
        col: 0,
        row: 0,
        col_span: 0,
        row_span: 0,
        overlay: true,
    }
}

// Sub-windows are numbered clockwise from main, not in reading order. Checked
// on a U4323QE across 0x31, 0x32, 0x33, 0x35 and 0x41;
// `geometry_orders_sub_windows_clockwise_from_main` holds the tables to it.
const C_FULL: &[Cell] = &[cell(0, 0, 0, 1, 1)];
const C_PIP: &[Cell] = &[cell(0, 0, 0, 1, 1), inset(1)];
/// Main left, sub1 right.
const C_2H: &[Cell] = &[cell(0, 0, 0, 1, 1), cell(1, 1, 0, 1, 1)];
/// Main top, sub1 bottom.
const C_2V: &[Cell] = &[cell(0, 0, 0, 1, 1), cell(1, 0, 1, 1, 1)];
/// Main full-height left, stacked right column.
const C_L1R2: &[Cell] = &[
    cell(0, 0, 0, 1, 2),
    cell(1, 1, 0, 1, 1),
    cell(2, 1, 1, 1, 1),
];
/// Stacked left column, main full-height right. sub1 is bottom-left.
const C_L2R1: &[Cell] = &[
    cell(0, 1, 0, 1, 2),
    cell(1, 0, 1, 1, 1),
    cell(2, 0, 0, 1, 1),
];
/// Main full-width top, two below. sub1 is bottom-right.
const C_T1B2: &[Cell] = &[
    cell(0, 0, 0, 2, 1),
    cell(1, 1, 1, 1, 1),
    cell(2, 0, 1, 1, 1),
];
/// Two on top, main full-width bottom. sub1 is top-left.
const C_T2B1: &[Cell] = &[
    cell(0, 0, 1, 2, 1),
    cell(1, 0, 0, 1, 1),
    cell(2, 1, 0, 1, 1),
];
const C_3COL: &[Cell] = &[
    cell(0, 0, 0, 1, 1),
    cell(1, 1, 0, 1, 1),
    cell(2, 2, 0, 1, 1),
];
const C_4COL: &[Cell] = &[
    cell(0, 0, 0, 1, 1),
    cell(1, 1, 0, 1, 1),
    cell(2, 2, 0, 1, 1),
    cell(3, 3, 0, 1, 1),
];
/// Quadrants, main top-left, then clockwise.
const C_QUAD: &[Cell] = &[
    cell(0, 0, 0, 1, 1),
    cell(1, 1, 0, 1, 1),
    cell(2, 1, 1, 1, 1),
    cell(3, 0, 1, 1, 1),
];

/// The shape of a layout, named the way Dell's software names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    Fullscreen,
    /// One inset over a full-screen main window.
    PipInset {
        large: bool,
    },
    SideBySide,
    Stacked,
    /// L1R2.
    LeftOneRightTwo,
    /// L2R1.
    LeftTwoRightOne,
    /// T1B2.
    TopOneBottomTwo,
    /// T2B1.
    TopTwoBottomOne,
    ThreeColumns,
    FourColumns,
    Quadrants,
    Unknown,
}

impl Arrangement {
    pub fn label(self) -> &'static str {
        match self {
            Arrangement::Fullscreen => "fullscreen",
            Arrangement::PipInset { large: false } => "pip inset (small)",
            Arrangement::PipInset { large: true } => "pip inset (large)",
            Arrangement::SideBySide => "side by side",
            Arrangement::Stacked => "stacked",
            Arrangement::LeftOneRightTwo => "left full-height + right column",
            Arrangement::LeftTwoRightOne => "left column + right full-height",
            Arrangement::TopOneBottomTwo => "top full-width + bottom row",
            Arrangement::TopTwoBottomOne => "top row + bottom full-width",
            Arrangement::ThreeColumns => "three columns",
            Arrangement::FourColumns => "four columns",
            Arrangement::Quadrants => "2x2 quadrants",
            Arrangement::Unknown => "unknown",
        }
    }
}

/// Everything knowable about a layout without touching a panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The value as given, write-aliases included.
    pub layout: u8,
    /// What the panel reports after that value is written from off.
    pub canonical: u8,
    pub arrangement: Arrangement,
    /// `(cols, rows)`. `(1, 1)` for PiP, where the inset is an overlay.
    pub grid: (u8, u8),
    pub cells: &'static [Cell],
    pub panes: Panes,
    /// How far the shape is trusted. [`Panes::proven`] covers the pane count.
    pub provenance: Provenance,
    /// A caveat to print alongside, if any.
    pub note: Option<&'static str>,
}

const NOTE_ALIAS: &str =
    "0x01/0x02 are these layouts only when written from off. From PiP they're step actions \
     (0x02 moves the inset, 0x01 resizes it). This assumes off; write the explicit layout \
     code to mean a layout.";
const NOTE_INFERRED: &str = "from Dell's layout table; not measured on a panel.";
const NOTE_PIP: &str =
    "the inset's corner isn't readable over DDC; it's an overlay with no grid position.";

/// One row of the layout table: codes `lo..=hi` share a shape.
struct Row {
    lo: u8,
    hi: u8,
    arrangement: Arrangement,
    grid: (u8, u8),
    cells: &'static [Cell],
    provenance: Provenance,
    note: Option<&'static str>,
}

const fn row(
    lo: u8,
    hi: u8,
    arrangement: Arrangement,
    grid: (u8, u8),
    cells: &'static [Cell],
    measured: bool,
) -> Row {
    let (provenance, note) = if measured {
        (Provenance::Observed, None)
    } else {
        (Provenance::Inferred, Some(NOTE_INFERRED))
    };
    Row {
        lo,
        hi,
        arrangement,
        grid,
        cells,
        provenance,
        note,
    }
}

/// Every known layout. The measured ones are the U4323QE's.
const LAYOUTS: &[Row] = &[
    row(0x00, 0x00, Arrangement::Fullscreen, (1, 1), C_FULL, true),
    Row {
        note: Some(NOTE_PIP),
        ..row(
            0x21,
            0x21,
            Arrangement::PipInset { large: false },
            (1, 1),
            C_PIP,
            true,
        )
    },
    Row {
        note: Some(NOTE_PIP),
        ..row(
            0x22,
            0x22,
            Arrangement::PipInset { large: true },
            (1, 1),
            C_PIP,
            true,
        )
    },
    row(0x23, 0x23, Arrangement::SideBySide, (2, 1), C_2H, false),
    row(0x24, 0x24, Arrangement::SideBySide, (2, 1), C_2H, true),
    row(0x25, 0x2E, Arrangement::SideBySide, (2, 1), C_2H, false),
    row(0x2F, 0x2F, Arrangement::Stacked, (1, 2), C_2V, true),
    row(
        0x31,
        0x31,
        Arrangement::LeftOneRightTwo,
        (2, 2),
        C_L1R2,
        true,
    ),
    row(
        0x32,
        0x32,
        Arrangement::LeftTwoRightOne,
        (2, 2),
        C_L2R1,
        true,
    ),
    row(
        0x33,
        0x33,
        Arrangement::TopOneBottomTwo,
        (2, 2),
        C_T1B2,
        true,
    ),
    row(0x34, 0x34, Arrangement::ThreeColumns, (3, 1), C_3COL, true),
    row(
        0x35,
        0x35,
        Arrangement::TopTwoBottomOne,
        (2, 2),
        C_T2B1,
        true,
    ),
    row(0x36, 0x36, Arrangement::ThreeColumns, (3, 1), C_3COL, false),
    row(0x41, 0x41, Arrangement::Quadrants, (2, 2), C_QUAD, true),
    row(0x42, 0x42, Arrangement::FourColumns, (4, 1), C_4COL, false),
    row(0x51, 0x51, Arrangement::Stacked, (1, 2), C_2V, false),
];

/// Geometry for any 0xE9 byte. Write-aliases are folded (with a note
/// attached) and an unknown byte comes back as [`Arrangement::Unknown`].
/// Whether a given panel accepts it is [`Geometry::advertised`].
pub fn geometry(layout: u8) -> Geometry {
    let canonical = pip::canonical(layout);
    let found = LAYOUTS.iter().find(|r| (r.lo..=r.hi).contains(&canonical));
    let (arrangement, grid, cells, provenance, note) = match found {
        Some(r) => (r.arrangement, r.grid, r.cells, r.provenance, r.note),
        None => (
            Arrangement::Unknown,
            (0, 0),
            &[][..],
            Provenance::Unknown,
            None,
        ),
    };
    Geometry {
        layout,
        canonical,
        arrangement,
        grid,
        cells,
        panes: bits::panes(canonical),
        provenance,
        note: if canonical != layout {
            Some(NOTE_ALIAS)
        } else {
            note
        },
    }
}

impl Geometry {
    /// How many windows this layout has (the permissive count guards use).
    pub fn window_count(&self) -> u8 {
        self.panes.bound()
    }

    /// How many of 0xE8's three fields this layout uses.
    pub fn sub_slots(&self) -> u8 {
        self.panes.sub_slots()
    }

    pub fn has_window(&self, w: Window) -> bool {
        self.panes.has_window(w.index())
    }

    /// The windows this layout has, in index order.
    pub fn windows(&self) -> Vec<Window> {
        Window::ALL
            .into_iter()
            .filter(|w| self.has_window(*w))
            .collect()
    }

    /// Where a window sits, if the layout has it.
    pub fn cell(&self, w: Window) -> Option<Cell> {
        self.cells.iter().copied().find(|c| c.window == w.index())
    }

    pub fn pane_count_proven(&self) -> bool {
        self.panes.proven()
    }

    /// Whether the panel with these capabilities accepts this layout.
    pub fn advertised(&self, caps: &Capabilities) -> bool {
        match caps.legal_values(Vcp::PIP_MODE) {
            Some(v) if !v.is_empty() => v.contains(&self.layout),
            // No value list says nothing about which layouts are legal.
            _ => caps.supports(Vcp::PIP_MODE),
        }
    }

    /// The profile's name for this layout, if it has one.
    pub fn name(&self, panel: &Panel) -> Option<&'static str> {
        panel.value_name(Vcp::PIP_MODE, self.canonical)
    }

    /// One line with the layout named through `panel` and its caveats.
    pub fn describe(&self, panel: &Panel) -> String {
        let mut out = format!(
            "0x{:02X} {} — {} window(s), {}",
            self.canonical,
            self.name(panel).unwrap_or("?"),
            self.window_count(),
            self.arrangement.label()
        );
        if !self.pane_count_proven() {
            out.push_str(" [pane count unproven]");
        }
        if self.provenance != Provenance::Observed {
            out.push_str(&format!(" [{}]", self.provenance.label()));
        }
        out
    }
}

/// Geometry for every layout in the table, in code order.
pub fn all_layouts() -> Vec<Geometry> {
    LAYOUTS
        .iter()
        .flat_map(|r| r.lo..=r.hi)
        .map(geometry)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{idle_snapshot as live, U4323QE};
    use crate::guard::{refusals_of, warnings_of};
    use crate::plan::Step;
    use crate::vcp::input;

    // ---- windows --------------------------------------------------------

    #[test]
    fn window_indices_are_the_ones_both_registers_use() {
        assert_eq!(Window::Main.index(), 0);
        assert_eq!(Window::Sub3.index(), 3);
        assert_eq!(Window::Main.sub_field(), None);
        assert_eq!(Window::Sub1.sub_field(), Some(0));
        assert_eq!(Window::Sub3.sub_field(), Some(2));
    }

    #[test]
    fn window_names_parse() {
        for w in Window::ALL {
            assert_eq!(w.name().parse::<Window>(), Ok(w));
            assert_eq!(w.index().to_string().parse::<Window>(), Ok(w));
        }
        assert_eq!("SUB2".parse::<Window>(), Ok(Window::Sub2));
        assert_eq!("s1".parse::<Window>(), Ok(Window::Sub1));
        assert!("sub4".parse::<Window>().is_err());
        assert!("4".parse::<Window>().is_err());
    }

    // ---- 0xE5 -----------------------------------------------------------

    #[test]
    fn swap_encodes_the_one_word_verified_on_hardware() {
        assert_eq!(swap_word(Window::Main, Window::Sub1), SWAP_MAIN_SUB1);
        // Symmetric.
        assert_eq!(swap_word(Window::Sub1, Window::Main), 0xF010);
        assert_eq!(swap_word(Window::Sub1, Window::Sub2), 0xF021);
        assert_eq!(swap_word(Window::Sub2, Window::Sub3), 0xF032);
        assert_eq!(swap_word(Window::Main, Window::Sub3), 0xF030);
        // The zoom step isn't a swap of windows 0 and 2.
        assert_ne!(ZOOM_STEP, swap_word(Window::Main, Window::Sub2));
    }

    #[test]
    fn swap_plan_is_one_write_with_nothing_to_verify_or_undo() {
        let p = swap_plan(Window::Main, Window::Sub1);
        assert_eq!(p.writes(), vec![(0xE5, 0xF010)]);
        assert!(!p.steps.iter().any(|s| matches!(s, Step::Verify { .. })));
        assert!(p.snapshot.is_empty());
        // Nothing was snapshotted, so a runner captures nothing to undo.
        assert!(p.undo_from(&[]).is_none());
    }

    #[test]
    fn swap_with_release_adds_a_delayed_second_write() {
        let p = swap_plan_with_release(Window::Main, Window::Sub1);
        assert_eq!(p.writes(), vec![(0xE5, 0xF010), (0xE5, SWAP_RELEASE)]);
        assert_eq!(p.steps[1], Step::Dwell(SWAP_RELEASE_DELAY));
    }

    #[test]
    fn zoom_and_underscan_are_single_writes_to_the_action_register() {
        assert_eq!(zoom_plan().writes(), vec![(0xE5, 0x0002)]);
        assert_eq!(underscan_plan().writes(), vec![(0xE5, 0x0003)]);
        for p in [zoom_plan(), underscan_plan()] {
            assert!(p.snapshot.is_empty());
            assert!(!p.steps.iter().any(|s| matches!(s, Step::Verify { .. })));
        }
    }

    #[test]
    fn inset_step_writes_the_layout_register_not_the_action_one() {
        // 0xE9 = 0x02, not 0xE5 = 0x02 (which zooms).
        let p = cycle_inset_plan();
        assert_eq!(p.writes(), vec![(Vcp::PIP_MODE, 0x0002)]);
        assert!(p.snapshot.is_empty());
        assert!(p.undo_from(&[]).is_none());
    }

    // ---- 0xE8 -----------------------------------------------------------

    #[test]
    fn every_sub_window_is_addressable() {
        // 0x6DF1 = [hdmi1, dp1, usb-c].
        let base = 0x6DF1;
        let w = with_sub(base, Window::Sub2, input::DP2).unwrap();
        assert_eq!(sub_sources(w), [input::HDMI_1, input::DP2, input::USB_C]);
        let w = with_sub(base, Window::Sub3, input::HDMI_2).unwrap();
        assert_eq!(
            sub_sources(w),
            [input::HDMI_1, input::DISPLAY_PORT, input::HDMI_2]
        );
        assert_eq!(w & 0x8000, 0);
        assert_eq!(with_sub(base, Window::Main, input::DP2), None);
    }

    #[test]
    fn sub1_addressing_matches_the_existing_codec() {
        for input in [
            input::DISPLAY_PORT,
            input::DP2,
            input::HDMI_1,
            input::HDMI_2,
            input::USB_C,
        ] {
            assert_eq!(
                with_sub(0x6DF3, Window::Sub1, input),
                Some(pip_sub::with_sub1(0x6DF3, input))
            );
        }
        assert_eq!(
            with_sub(0x6DF3, Window::Sub1, input::DISPLAY_PORT),
            Some(0x6DEF)
        );
    }

    #[test]
    fn several_assignments_compose_and_main_is_skipped() {
        let w = apply_subs(
            0x6DF1,
            &[
                (Window::Sub1, input::DP2),
                (Window::Main, input::USB_C),
                (Window::Sub3, input::HDMI_2),
            ],
        );
        assert_eq!(
            sub_sources(w),
            [input::DP2, input::DISPLAY_PORT, input::HDMI_2]
        );
    }

    #[test]
    fn a_sub_source_plan_verifies_the_readback() {
        let p = sub_source_plan(0x6DF1, Window::Sub2, input::DP2).unwrap();
        let word = with_sub(0x6DF1, Window::Sub2, input::DP2).unwrap();
        assert_eq!(p.writes(), vec![(Vcp::PIP_SUB_SOURCE, word)]);
        assert_eq!(
            *p.steps.last().unwrap(),
            Step::Verify {
                vcp: 0xE8,
                expect: word
            }
        );
        assert!(p.snapshot.contains(&Vcp::PIP_MODE));
        let undo = p.undo_from(&[(0xE9, 0x0024), (0xE8, 0x6DF1)]).unwrap();
        assert_eq!(undo.writes(), vec![(Vcp::PIP_SUB_SOURCE, 0x6DF1)]);
        assert!(sub_source_plan(0x6DF1, Window::Main, input::DP2).is_none());
    }

    #[test]
    fn apply_plan_orders_layout_then_input_then_subs_and_waits_between() {
        let p = apply_plan(
            pip::QUAD_SELF_LEFT_COLUMN,
            input::USB_C,
            0x6DF1,
            &[(Window::Sub2, input::DP2)],
        );
        let word = with_sub(0x6DF1, Window::Sub2, input::DP2).unwrap();
        assert_eq!(
            p.writes(),
            vec![
                (Vcp::PIP_MODE, 0x0041),
                (Vcp::INPUT_SOURCE, 0x001B),
                (Vcp::PIP_SUB_SOURCE, word)
            ]
        );
        assert_eq!(
            p.steps[1],
            Step::AwaitReady {
                vcp: Vcp::PIP_MODE,
                timeout: LAYOUT_SETTLE
            }
        );
        assert_eq!(
            p.steps[3],
            Step::AwaitReady {
                vcp: Vcp::INPUT_SOURCE,
                timeout: LAYOUT_SETTLE
            }
        );
        assert_eq!(p.total_dwell(), Duration::ZERO);
    }

    // ---- the inset corner -----------------------------------------------

    #[test]
    fn the_inset_corner_always_prints_its_caveat() {
        let mut t = InsetTracker::default();
        t.step();
        let c = t.corner();
        assert_eq!(t.steps(), 1);
        assert_eq!(c.guess(), "left-top");
        let shown = c.to_string();
        assert!(shown.contains("left-top"));
        assert!(shown.contains("unverified"));
        assert!(c.because().contains("isn't readable over DDC"));
    }

    #[test]
    fn the_corner_cycle_wraps_and_can_be_resumed() {
        let mut t = InsetTracker::default();
        for _ in 0..4 {
            t.step();
        }
        assert_eq!(t.corner().guess(), INSET_CORNERS[0]);
        assert_eq!(InsetTracker::resumed(2, 1).corner().guess(), "left-bottom");
        assert_eq!(
            InsetTracker::resumed(9, 0).corner().guess(),
            INSET_CORNERS[1]
        );
    }

    #[test]
    fn inset_step_is_refused_outside_pip() {
        let r = refusals_of(&check_inset_cycle(&live()));
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("picture-in-picture"));
        assert!(r[0].contains("Nothing was written"));

        for layout in [pip::PIP_SMALL, pip::PIP_LARGE] {
            let mut s = live();
            s.layout = Some(layout as u16);
            assert!(check_inset_cycle(&s).is_empty(), "0x{layout:02X}");
        }
    }

    #[test]
    fn inset_step_is_refused_when_the_panel_does_not_advertise_it() {
        let s = Snapshot::from_reads(
            crate::vcp::default_panel(),
            Capabilities::parse("(vcp(60( 1B 0F) E9(00 21 22 24) E8 E5)mccs_ver(2.1))"),
            &[(0xE9, 0x0021), (0xF2, 0x0000)],
        );
        let r = refusals_of(&check_inset_cycle(&s));
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("0x02"));
    }

    #[test]
    fn an_unreadable_layout_warns_rather_than_allowing_silently() {
        let mut s = live();
        s.layout = None;
        let v = check_inset_cycle(&s);
        assert!(refusals_of(&v).is_empty());
        assert!(warnings_of(&v).iter().any(|w| w.contains("0xE9")));
    }

    // ---- composed checks ------------------------------------------------

    #[test]
    fn swap_check_adds_dells_two_window_gate_as_a_warning_only() {
        let mut s = live();
        s.layout = Some(pip::QUAD_SELF_LEFT_COLUMN as u16);
        let v = check_swap(&s);
        assert!(refusals_of(&v).is_empty());
        assert!(warnings_of(&v)[0].contains("doesn't offer a swap"));
        assert!(check_swap(&live()).is_empty());
        assert!(!ddpm_offers_swap(pip::OFF));
        assert!(ddpm_offers_swap(0x24));
        assert!(!ddpm_offers_swap(0x41));
    }

    #[test]
    fn apply_checks_sub_windows_against_the_layout_being_moved_to() {
        // In the current 2-up layout sub2 would be refused.
        let s = live();
        assert!(!refusals_of(&guard::evaluate(
            &s,
            &Intent::SetSubSource {
                window: 2,
                input: input::DP2
            }
        ))
        .is_empty());

        let v = check_apply(
            &s,
            pip::QUAD_SELF_LEFT_COLUMN,
            input::USB_C,
            &[(Window::Sub2, input::DP2), (Window::Sub3, input::HDMI_2)],
        );
        assert!(refusals_of(&v).is_empty(), "{v:?}");
    }

    #[test]
    fn a_duplicate_in_the_end_state_is_mentioned_but_allowed() {
        // Moving to quad exposes sub3 (usb-c) while main stays usb-c.
        let v = check_apply(
            &live(),
            pip::QUAD_SELF_LEFT_COLUMN,
            input::USB_C,
            &[(Window::Sub2, input::DP2)],
        );
        assert!(refusals_of(&v).is_empty(), "{v:?}");
        assert!(
            warnings_of(&v).iter().any(|m| m.contains("appear twice")),
            "{v:?}"
        );

        let v = check_apply(
            &live(),
            pip::PBP_2UP_SELF_LEFT,
            input::DP2,
            &[(Window::Sub1, input::DP2)],
        );
        assert!(refusals_of(&v).is_empty(), "{v:?}");
        assert!(
            warnings_of(&v).iter().any(|m| m.contains("appear twice")),
            "{v:?}"
        );
    }

    #[test]
    fn apply_refuses_an_assignment_aimed_at_the_main_window() {
        let v = check_apply(
            &live(),
            pip::PBP_2UP_SELF_LEFT,
            input::USB_C,
            &[(Window::Main, 0x0F)],
        );
        assert!(refusals_of(&v).iter().any(|m| m.contains("VCP 0x60")));
    }

    #[test]
    fn apply_refuses_an_alias_that_is_a_step_action_in_pip() {
        let mut s = live();
        s.layout = Some(pip::PIP_SMALL as u16);
        let v = check_apply(&s, 0x02, input::USB_C, &[]);
        assert!(
            refusals_of(&v).iter().any(|m| m.contains("corner-step")),
            "{v:?}"
        );
    }

    #[test]
    fn apply_says_busy_once() {
        let mut s = live();
        s.status = Some(0x0080);
        let r = refusals_of(&check_apply(
            &s,
            pip::PIP_SMALL,
            input::USB_C,
            &[(Window::Sub1, input::DP2)],
        ));
        assert_eq!(r.iter().filter(|m| m.contains("busy")).count(), 1);
    }

    // ---- geometry -------------------------------------------------------

    #[test]
    fn geometry_needs_no_panel_and_answers_for_every_byte() {
        let g = geometry(pip::PBP_2UP_SELF_LEFT);
        assert_eq!(g.arrangement, Arrangement::SideBySide);
        assert_eq!(g.grid, (2, 1));
        assert_eq!(g.window_count(), 2);
        assert_eq!(g.sub_slots(), 1);
        assert_eq!(g.cell(Window::Main).unwrap().col, 0);
        assert_eq!(g.cell(Window::Sub1).unwrap().col, 1);
        assert_eq!(g.cell(Window::Sub2), None);
        assert_eq!(g.provenance, Provenance::Observed);

        let u = geometry(0x99);
        assert_eq!(u.arrangement, Arrangement::Unknown);
        assert!(u.cells.is_empty());
        assert_eq!(u.provenance, Provenance::Unknown);
        assert_eq!(u.window_count(), 4);
    }

    #[test]
    fn geometry_folds_the_write_aliases_and_says_so() {
        let g = geometry(0x02);
        assert_eq!(g.layout, 0x02);
        assert_eq!(g.canonical, pip::PBP_2UP_SELF_LEFT);
        assert_eq!(g.arrangement, geometry(0x24).arrangement);
        assert_eq!(g.note, Some(NOTE_ALIAS));
        assert!(geometry(0x24).note.is_none());
    }

    #[test]
    fn all_layouts_covers_the_table() {
        let codes: Vec<u8> = all_layouts().iter().map(|g| g.canonical).collect();
        for c in pip::CANONICAL {
            assert!(codes.contains(&c), "0x{c:02X}");
        }
        assert!(codes.contains(&0x42) && codes.contains(&0x2E));
        assert!(codes.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }

    #[test]
    fn every_layout_places_exactly_the_windows_it_claims_to_have() {
        for g in all_layouts() {
            let n = g.window_count() as usize;
            assert_eq!(
                g.cells.len(),
                n,
                "0x{:02X} {:?}",
                g.canonical,
                g.arrangement
            );
            for w in g.windows() {
                assert!(
                    g.cell(w).is_some(),
                    "0x{:02X} has no cell for {w}",
                    g.canonical
                );
            }
            let mut idx: Vec<u8> = g.cells.iter().map(|c| c.window).collect();
            idx.sort_unstable();
            assert_eq!(
                idx,
                (0..n as u8).collect::<Vec<_>>(),
                "0x{:02X}",
                g.canonical
            );
        }
    }

    #[test]
    fn grid_cells_fit_inside_their_grid_and_do_not_overlap() {
        for g in all_layouts() {
            let (cols, rows) = g.grid;
            let mut occupied: Vec<(u8, u8)> = Vec::new();
            for c in g.cells.iter().filter(|c| !c.overlay) {
                assert!(
                    c.col + c.col_span <= cols,
                    "0x{:02X} cell {c:?}",
                    g.canonical
                );
                assert!(
                    c.row + c.row_span <= rows,
                    "0x{:02X} cell {c:?}",
                    g.canonical
                );
                for col in c.col..c.col + c.col_span {
                    for row in c.row..c.row + c.row_span {
                        assert!(
                            !occupied.contains(&(col, row)),
                            "0x{:02X}: overlap",
                            g.canonical
                        );
                        occupied.push((col, row));
                    }
                }
            }
        }
    }

    #[test]
    fn pip_puts_the_inset_over_the_main_window() {
        for (code, large) in [(pip::PIP_SMALL, false), (pip::PIP_LARGE, true)] {
            let g = geometry(code);
            assert_eq!(g.arrangement, Arrangement::PipInset { large });
            assert_eq!(g.grid, (1, 1));
            assert!(!g.cell(Window::Main).unwrap().overlay);
            let inset = g.cell(Window::Sub1).unwrap();
            assert!(inset.overlay);
            assert_eq!((inset.col_span, inset.row_span), (0, 0));
            assert!(layout_has_inset(code));
            assert!(g.note.unwrap().contains("isn't readable"));
        }
        assert!(!layout_has_inset(pip::PBP_2UP_SELF_LEFT));
    }

    #[test]
    fn the_three_pane_layouts_are_measured_with_clockwise_cells() {
        for code in [pip::PBP_L2R1, pip::PBP_VSTACK_SELF_TOP, pip::PBP_T2B1] {
            let g = geometry(code);
            assert_eq!(g.provenance, Provenance::Observed, "0x{code:02X}");
            assert!(g.pane_count_proven(), "0x{code:02X}");
            assert_eq!(g.window_count(), 3, "0x{code:02X}");
            assert_eq!(g.note, None);
        }
        assert_eq!(
            geometry(pip::PBP_L2R1).cell(Window::Sub1).unwrap().row,
            1,
            "bottom-left"
        );
        assert_eq!(
            geometry(pip::PBP_VSTACK_SELF_TOP)
                .cell(Window::Sub1)
                .unwrap()
                .col,
            1
        );
        let quad = geometry(pip::QUAD_SELF_LEFT_COLUMN);
        let at = |w| quad.cell(w).map(|c| (c.col, c.row));
        assert_eq!(at(Window::Sub2), Some((1, 1)));
        assert_eq!(at(Window::Sub3), Some((0, 1)));
    }

    #[test]
    fn advertised_is_decided_by_the_caps_not_the_table() {
        let caps = Capabilities::parse(U4323QE);
        assert!(geometry(pip::QUAD_SELF_LEFT_COLUMN).advertised(&caps));
        assert!(
            geometry(0x02).advertised(&caps),
            "the write-aliases are legal writes"
        );
        for code in [0x23u8, 0x36, 0x42, 0x51] {
            assert!(!geometry(code).advertised(&caps), "0x{code:02X}");
            assert_eq!(geometry(code).note, Some(NOTE_INFERRED));
            assert_eq!(geometry(code).provenance, Provenance::Inferred);
        }
        assert!(!NOTE_INFERRED.contains("this panel"));
    }

    #[test]
    fn geometry_pane_counts_agree_with_the_bits_module() {
        for g in all_layouts() {
            assert_eq!(
                g.window_count(),
                bits::panes(g.canonical).bound(),
                "0x{:02X}",
                g.canonical
            );
            assert_eq!(g.sub_slots() as usize, g.window_count() as usize - 1);
        }
    }

    /// Sub-windows run clockwise from main in every 2x2 layout.
    #[test]
    fn geometry_orders_sub_windows_clockwise_from_main() {
        const CW: [(u8, u8); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];
        let mut checked = 0;
        for g in all_layouts() {
            if g.grid != (2, 2) || g.cells.iter().any(|c| c.overlay) {
                continue;
            }
            let mut owner = [None; 4];
            for c in g.cells {
                for (i, &(col, row)) in CW.iter().enumerate() {
                    if (c.col..c.col + c.col_span).contains(&col)
                        && (c.row..c.row + c.row_span).contains(&row)
                    {
                        owner[i] = Some(c.window);
                    }
                }
            }
            let owner: Vec<u8> = owner
                .iter()
                .map(|o| {
                    o.unwrap_or_else(|| panic!("0x{:02X} leaves a quadrant unowned", g.canonical))
                })
                .collect();
            let start = (0..4)
                .find(|&i| owner[i] == 0 && owner[(i + 1) % 4] != 0)
                .unwrap_or_else(|| panic!("0x{:02X}: main isn't one block", g.canonical));
            let subs = (1..=4)
                .map(|step| owner[(start + step) % 4])
                .take_while(|w| *w != 0);
            for (expect, w) in (1u8..).zip(subs) {
                assert_eq!(w, expect, "0x{:02X} breaks clockwise order", g.canonical);
            }
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} layouts exercised the rule");
    }

    #[test]
    fn geometry_renders_its_own_caveats() {
        let shown = geometry(0x23).describe(crate::vcp::default_panel());
        assert!(shown.contains("unproven"), "{shown}");
        assert!(shown.contains("inferred"), "{shown}");
        for code in [pip::PBP_2UP_SELF_LEFT, pip::PBP_L2R1, pip::PBP_T2B1] {
            let clean = geometry(code).describe(crate::vcp::default_panel());
            assert!(
                !clean.contains("unproven") && !clean.contains("inferred"),
                "{clean}"
            );
        }
    }
}
