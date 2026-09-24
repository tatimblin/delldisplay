//! What the panel shows, pane by pane, in words an agent can use.
//!
//! [`crate::pxp`] speaks in windows (`main`, `sub1`..) and codes. This
//! module names layouts by shape ([`layout_name`]) and panes by where they sit
//! ([`positions`]: `left`, `top-right`, `inset`), reads the whole picture as a
//! [`View`], and turns a request like "side-by-side, this computer on the
//! right" into the view to write ([`resolve`]). [`hidden_by`] is the check
//! that keeps a change from taking anything off the screen.
//!
//! No OS calls and no traffic: everything here is decided from a snapshot.

use std::collections::BTreeSet;

use crate::bits::{self, Arrival};
use crate::guard::Snapshot;
use crate::pxp::{self, Arrangement, Cell, Geometry, Window};

/// A layout's name, from its shape. `None` for a byte the table doesn't know.
pub fn layout_name(g: &Geometry) -> Option<&'static str> {
    Some(match g.arrangement {
        Arrangement::Fullscreen => "full",
        Arrangement::PipInset { large: false } => "pip-small",
        Arrangement::PipInset { large: true } => "pip-large",
        Arrangement::SideBySide => "side-by-side",
        Arrangement::Stacked => "stacked",
        Arrangement::LeftOneRightTwo => "left-1-right-2",
        Arrangement::LeftTwoRightOne => "left-2-right-1",
        Arrangement::TopOneBottomTwo => "top-1-bottom-2",
        Arrangement::TopTwoBottomOne => "top-2-bottom-1",
        Arrangement::ThreeColumns => "three-columns",
        Arrangement::FourColumns => "four-columns",
        Arrangement::Quadrants => "quad",
        Arrangement::Unknown => return None,
    })
}

/// Where one cell sits: `left`, `top-right`, `inset`, or `full` for a lone
/// pane. A cell spanning a whole axis says nothing about that axis, so the
/// tall left pane of left-1-right-2 is just `left`.
pub fn position(g: &Geometry, c: Cell) -> String {
    if c.overlay {
        return String::from("inset");
    }
    let (cols, rows) = g.grid;
    let across = axis(
        c.col,
        c.col_span,
        cols,
        ["left", "middle", "right"],
        "column",
    );
    let down = axis(c.row, c.row_span, rows, ["top", "middle", "bottom"], "row");
    match (down, across) {
        (Some(d), Some(a)) => format!("{d}-{a}"),
        (Some(p), None) | (None, Some(p)) => p,
        // The main window of a PiP layout sits under the inset.
        (None, None) if matches!(g.arrangement, Arrangement::PipInset { .. }) => {
            String::from("main")
        }
        (None, None) => String::from("full"),
    }
}

fn axis(at: u8, span: u8, n: u8, words: [&str; 3], counted: &str) -> Option<String> {
    if n <= 1 || span >= n {
        return None;
    }
    Some(match n {
        2 => [words[0], words[2]][at as usize % 2].to_string(),
        3 => words[at as usize % 3].to_string(),
        _ => format!("{counted}-{}", at + 1),
    })
}

/// Every window the layout has, with its position name, in window order.
pub fn positions(g: &Geometry) -> Vec<(Window, String)> {
    g.windows()
        .into_iter()
        .filter_map(|w| g.cell(w).map(|c| (w, position(g, c))))
        .collect()
}

/// Which input this computer is plugged into, from 0x60's high byte: the port
/// the request arrived on. `None` when the panel didn't say (another model,
/// the USB tunnel, a garbled word).
pub fn this_input(raw_0x60: u16) -> Option<u8> {
    let w = bits::input_word(raw_0x60);
    match w.arrival {
        Arrival::Port(p) if w.is_valid() => Some(p),
        _ => None,
    }
}

/// The three registers that decide what's on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// 0xE9, write-aliases folded.
    pub layout: u8,
    /// 0x60's low byte: the main window's input.
    pub main: u8,
    /// 0xE8, all three fields, stale ones included.
    pub sub: u16,
}

/// One pane of a [`View`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub window: Window,
    pub position: String,
    /// An input code; 0 means nothing is assigned.
    pub input: u8,
}

impl View {
    /// `None` if the snapshot is missing 0xE9, 0x60 or 0xE8.
    pub fn read(snap: &Snapshot) -> Option<View> {
        Some(View {
            layout: snap.layout_code()?,
            main: snap.main_input()?,
            sub: snap.sub?,
        })
    }

    pub fn geometry(&self) -> Geometry {
        pxp::geometry(self.layout)
    }

    /// What `w` shows, whether or not the layout has it.
    pub fn source(&self, w: Window) -> u8 {
        match w.sub_field() {
            None => self.main,
            Some(f) => pxp::sub_sources(self.sub)[f],
        }
    }

    /// The panes the layout actually has. Fields of 0xE8 the layout doesn't
    /// use are left out: they keep whatever was there last.
    pub fn panes(&self) -> Vec<Pane> {
        positions(&self.geometry())
            .into_iter()
            .map(|(window, position)| Pane {
                window,
                position,
                input: self.source(window),
            })
            .collect()
    }

    /// The inputs on screen somewhere.
    pub fn visible(&self) -> BTreeSet<u8> {
        self.panes()
            .into_iter()
            .map(|p| p.input)
            .filter(|i| *i != 0)
            .collect()
    }

    /// Same layout and the same source in every pane it has. Stale 0xE8
    /// fields don't count.
    pub fn same(&self, other: &View) -> bool {
        self.layout == other.layout
            && self
                .geometry()
                .windows()
                .into_iter()
                .all(|w| self.source(w) == other.source(w))
    }
}

/// A pane's source in a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The computer asking.
    This,
    /// Whatever the main window shows now.
    Current,
    Input(u8),
}

/// A resolved request: the view it ends in and the sub-window assignments
/// [`pxp::apply_plan`] takes to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arranged {
    pub view: View,
    pub subs: Vec<(Window, u8)>,
}

/// Turn a layout and `position -> source` picks into the view to write.
/// Positions left out keep their source; an unset main keeps the current
/// input. `this` is the asking computer's input, if known.
pub fn resolve(
    now: &View,
    layout: u8,
    picks: &[(String, Source)],
    this: Option<u8>,
) -> Result<Arranged, Vec<String>> {
    let g = pxp::geometry(layout);
    let name = layout_name(&g).unwrap_or("that layout");
    let places = positions(&g);
    let mut errors = Vec::new();
    let mut main = now.main;
    let mut subs = Vec::new();
    let mut seen = BTreeSet::new();
    for (at, source) in picks {
        let Some((window, _)) = places.iter().find(|(_, p)| p == at) else {
            let names: Vec<&str> = places.iter().map(|(_, p)| p.as_str()).collect();
            errors.push(format!(
                "'{at}' isn't a position in {name}; it has {}",
                names.join(", ")
            ));
            continue;
        };
        if !seen.insert(*window) {
            errors.push(format!("'{at}' is given twice"));
            continue;
        }
        let input = match *source {
            Source::Input(i) => i,
            Source::Current => now.main,
            Source::This => match this {
                Some(i) => i,
                None => {
                    errors.push(String::from(
                        "which input this computer is on isn't known, so `self` can't be \
                         placed; set `self_input` in the config",
                    ));
                    continue;
                }
            },
        };
        match window {
            Window::Main => main = input,
            w => subs.push((*w, input)),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Arranged {
        view: View {
            layout: g.canonical,
            main,
            sub: pxp::apply_subs(now.sub, &subs),
        },
        subs,
    })
}

/// The inputs on screen in `before` that `after` takes off it, not counting
/// `this` computer's own: it may always remove itself.
pub fn hidden_by(before: &View, after: &View, this: Option<u8>) -> Vec<u8> {
    let kept = after.visible();
    before
        .visible()
        .into_iter()
        .filter(|i| !kept.contains(i) && Some(*i) != this)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::idle_snapshot;
    use crate::vcp::input::{DISPLAY_PORT as DP1, DP2, HDMI_1, HDMI_2, USB_C};
    use crate::vcp::pip;

    fn names(layout: u8) -> Vec<String> {
        positions(&pxp::geometry(layout))
            .into_iter()
            .map(|(_, p)| p)
            .collect()
    }

    fn view(layout: u8, main: u8, subs: [u8; 3]) -> View {
        View {
            layout,
            main,
            sub: crate::vcp::pip_sub::encode(subs),
        }
    }

    #[test]
    fn every_layout_the_panel_accepts_has_a_name() {
        for code in pip::CANONICAL {
            assert!(layout_name(&pxp::geometry(code)).is_some(), "0x{code:02X}");
        }
        assert_eq!(layout_name(&pxp::geometry(0x24)), Some("side-by-side"));
        assert_eq!(layout_name(&pxp::geometry(0x7F)), None);
    }

    #[test]
    fn positions_follow_the_measured_tables() {
        // Window order, which is clockwise from main; see docs/pxp.md.
        assert_eq!(names(0x00), ["full"]);
        assert_eq!(names(0x21), ["main", "inset"]);
        assert_eq!(names(0x24), ["left", "right"]);
        assert_eq!(names(0x2F), ["top", "bottom"]);
        assert_eq!(names(0x31), ["left", "top-right", "bottom-right"]);
        assert_eq!(names(0x32), ["right", "bottom-left", "top-left"]);
        assert_eq!(names(0x33), ["top", "bottom-right", "bottom-left"]);
        assert_eq!(names(0x34), ["left", "middle", "right"]);
        assert_eq!(names(0x35), ["bottom", "top-left", "top-right"]);
        assert_eq!(
            names(0x41),
            ["top-left", "top-right", "bottom-right", "bottom-left"]
        );
        assert_eq!(
            names(0x42),
            ["column-1", "column-2", "column-3", "column-4"]
        );
    }

    #[test]
    fn position_names_are_unique_within_a_layout() {
        for g in pxp::all_layouts() {
            let n = names(g.layout);
            let set: BTreeSet<_> = n.iter().collect();
            assert_eq!(set.len(), n.len(), "0x{:02X}: {n:?}", g.layout);
        }
    }

    #[test]
    fn this_input_is_the_arrival_port() {
        assert_eq!(this_input(0x1313), Some(DP2), "this host owns main");
        assert_eq!(this_input(0x130F), Some(DP2), "another host owns main");
        assert_eq!(this_input(0x0013), None, "not reported");
        assert_eq!(this_input(0x8313), None, "USB tunnel");
        assert_eq!(this_input(0x9913), None, "garbled");
    }

    #[test]
    fn the_idle_panel_reads_as_two_panes() {
        let v = View::read(&idle_snapshot()).unwrap();
        let panes = v.panes();
        assert_eq!(panes.len(), 2);
        assert_eq!(
            (panes[0].position.as_str(), panes[0].input),
            ("left", USB_C)
        );
        assert_eq!(
            (panes[1].position.as_str(), panes[1].input),
            ("right", HDMI_1)
        );
        assert_eq!(v.visible(), [HDMI_1, USB_C].into());
    }

    #[test]
    fn stale_sub_fields_are_not_on_screen_and_dont_break_sameness() {
        let a = view(0x00, DP1, [HDMI_1, DP2, 0]);
        let b = view(0x00, DP1, [HDMI_2, 0, USB_C]);
        assert_eq!(a.visible(), [DP1].into());
        assert!(a.same(&b));
        assert!(!a.same(&view(0x24, DP1, [HDMI_1, 0, 0])));
    }

    #[test]
    fn resolve_places_this_computer_and_keeps_the_rest() {
        let now = view(0x00, DP1, [HDMI_1, DP2, USB_C]);
        let picks = [(String::from("right"), Source::This)];
        let got = resolve(&now, 0x24, &picks, Some(DP2)).unwrap();
        assert_eq!(got.view.layout, 0x24);
        assert_eq!(got.view.main, DP1, "an unset main keeps the current input");
        assert_eq!(got.subs, [(Window::Sub1, DP2)]);
        assert_eq!(got.view.source(Window::Sub1), DP2);
        assert_eq!(got.view.source(Window::Sub2), DP2, "untouched fields stay");
    }

    #[test]
    fn resolve_can_move_main_and_name_current() {
        let now = view(0x00, DP1, [0, 0, 0]);
        let picks = [
            (String::from("left"), Source::This),
            (String::from("right"), Source::Current),
        ];
        let got = resolve(&now, 0x24, &picks, Some(DP2)).unwrap();
        assert_eq!(got.view.main, DP2);
        assert_eq!(got.view.source(Window::Sub1), DP1);
    }

    #[test]
    fn resolve_folds_write_aliases() {
        let now = view(0x00, DP1, [0, 0, 0]);
        assert_eq!(resolve(&now, 0x02, &[], None).unwrap().view.layout, 0x24);
        assert_eq!(resolve(&now, 0x01, &[], None).unwrap().view.layout, 0x32);
    }

    #[test]
    fn resolve_reports_every_bad_pick() {
        let now = view(0x24, DP1, [HDMI_1, 0, 0]);
        let picks = [
            (String::from("middle"), Source::Current),
            (String::from("left"), Source::This),
            (String::from("right"), Source::Input(DP2)),
            (String::from("right"), Source::Input(HDMI_2)),
        ];
        let errs = resolve(&now, 0x24, &picks, None).unwrap_err();
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert!(errs[0].contains("left, right"), "{}", errs[0]);
        assert!(errs[1].contains("self_input"), "{}", errs[1]);
        assert!(errs[2].contains("twice"), "{}", errs[2]);
    }

    #[test]
    fn a_split_hides_nothing_and_a_takeover_does() {
        let before = view(0x00, DP1, [0, 0, 0]);
        let split = view(0x24, DP1, [DP2, 0, 0]);
        let takeover = view(0x00, DP2, [0, 0, 0]);
        assert!(hidden_by(&before, &split, Some(DP2)).is_empty());
        assert_eq!(hidden_by(&before, &takeover, Some(DP2)), [DP1]);
    }

    #[test]
    fn this_computer_may_always_take_itself_off() {
        let before = view(0x24, DP1, [DP2, 0, 0]);
        let after = view(0x00, DP1, [DP2, 0, 0]);
        assert!(hidden_by(&before, &after, Some(DP2)).is_empty());
        assert_eq!(hidden_by(&before, &after, None), [DP2]);
    }
}
