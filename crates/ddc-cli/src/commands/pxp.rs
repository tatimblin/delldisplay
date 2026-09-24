//! `pxp`: PiP/PBP layouts, sub-window sources and the 0xE5 window actions.
//!
//! `layouts` and `geometry` are arithmetic and never open the display. The
//! rest read one guard snapshot and run one plan through [`exec`]. 0xE5 is
//! write-only in practice and the inset corner can't be read, so those verbs
//! say what they can't confirm.

use clap::{Args as ClapArgs, Subcommand};
use ddc_core::guard::{Intent, Snapshot, Verdict};
use ddc_core::plan::Plan;
use ddc_core::pxp::{self, Window};
use ddc_core::vcp::{self, Vcp};
use ddc_transport::{Ddc, I2c, Runner};
use serde_json::{json, Value};

use super::args::{self, Write};
use super::exec::{self, exit, print_json, Ctx};

#[derive(ClapArgs, Debug)]
#[command(disable_help_subcommand = true)]
#[command(after_help = "Windows: main, sub1, sub2, sub3 (or 0-3)\n\
                        Inputs:  dp, dp2, hdmi1, hdmi2, usb-c (or 0xNN)\n\
                        Layouts: see `pxp layouts`")]
pub struct Args {
    #[command(subcommand)]
    pub verb: Option<Verb>,
}

#[derive(Subcommand, Debug)]
pub enum Verb {
    /// What the panel is showing, window by window (default).
    Status,
    /// Every layout with its window count and shape. Needs no display.
    Layouts,
    /// One layout in detail. Needs no display.
    Geometry {
        /// A name like `pip` or `quad`, or 0xNN.
        #[arg(value_parser = args::layout)]
        layout: u8,
    },
    /// Exchange two windows' sources (default: main and sub1).
    Swap {
        /// A window: main, sub1, sub2, sub3.
        #[arg(value_parser = args::window, default_value = "main")]
        a: Window,
        /// The other window.
        #[arg(value_parser = args::window, default_value = "sub1")]
        b: Window,
        /// Follow with Dell's release word (0xF000).
        #[arg(long)]
        release: bool,
        #[command(flatten)]
        write: Write,
    },
    /// Point a sub-window at an input.
    Sub {
        /// sub1, sub2 or sub3.
        #[arg(value_parser = args::sub_window)]
        window: Window,
        /// dp, dp2, hdmi1, hdmi2, usb-c or 0xNN.
        #[arg(value_parser = args::input)]
        input: u8,
        #[command(flatten)]
        write: Write,
    },
    /// Set the layout, main input and sub-window sources in one go.
    Apply {
        /// A name like `pip` or `quad`, or 0xNN.
        #[arg(value_parser = args::layout)]
        layout: u8,
        /// Main input; defaults to the current one.
        #[arg(long, value_parser = args::input, value_name = "INPUT")]
        main: Option<u8>,
        /// Source for sub1.
        #[arg(long, value_parser = args::input, value_name = "INPUT")]
        sub1: Option<u8>,
        /// Source for sub2.
        #[arg(long, value_parser = args::input, value_name = "INPUT")]
        sub2: Option<u8>,
        /// Source for sub3.
        #[arg(long, value_parser = args::input, value_name = "INPUT")]
        sub3: Option<u8>,
        #[command(flatten)]
        write: Write,
    },
    /// Move the PIP inset one corner on (PIP layouts only).
    Inset {
        /// The corner it's in now, if you know it, to name where it goes.
        #[arg(long, value_parser = args::corner)]
        from: Option<u8>,
        #[command(flatten)]
        write: Write,
    },
    /// Step the PBP zoom. Does nothing visible on the U4323QE.
    Zoom(Write),
    /// Toggle underscan. Does nothing visible on the U4323QE.
    Underscan(Write),
}

/// `layouts` and `geometry`, which need no display. Both name layouts with
/// the default profile, whose table this is.
pub fn offline(a: &Args, ctx: &Ctx) -> Option<u8> {
    match a.verb {
        Some(Verb::Layouts) => Some(show_layouts(ctx)),
        Some(Verb::Geometry { layout }) => Some(show_geometry(layout, ctx)),
        _ => None,
    }
}

pub fn run<T: I2c>(d: &mut Ddc<T>, a: &Args, ctx: &Ctx) -> u8 {
    if let Some(code) = offline(a, ctx) {
        return code;
    }
    let verb = a.verb.as_ref().unwrap_or(&Verb::Status);
    let snap = match Runner::new(d).read_snapshot() {
        Ok(s) => s,
        Err(e) => return exec::fail(ctx, &format!("couldn't read the panel: {e}")),
    };
    let (write, intent, plan, extra) = match build(verb, &snap) {
        Ok(Some(job)) => job,
        Ok(None) => return show_status(&snap, ctx),
        Err(e) => return exec::fail(ctx, &e),
    };
    let mut runner = ctx.runner(d, write);
    let (mut report, out) = exec::guarded(&mut runner, &snap, Some(&intent), &plan, extra, 1);
    if out.is_some() {
        report.notes.extend(follow_up(verb));
    }
    report.print(ctx)
}

type Job<'a> = (&'a Write, Intent, Plan, Vec<Verdict>);

/// The plan for a writing verb, the intent that guards it, and the checks the
/// guard can't make on its own. `Ok(None)` for `status`. `Err` means a
/// register the plan depends on couldn't be read.
fn build<'a>(verb: &'a Verb, snap: &Snapshot) -> Result<Option<Job<'a>>, String> {
    let sub_word = || {
        snap.sub
            .ok_or_else(|| String::from("couldn't read 0xE8, which holds all three sub sources"))
    };
    Ok(Some(match verb {
        Verb::Status | Verb::Layouts | Verb::Geometry { .. } => return Ok(None),
        Verb::Swap {
            a,
            b,
            release,
            write,
        } => {
            let plan = match release {
                true => pxp::swap_plan_with_release(*a, *b),
                false => pxp::swap_plan(*a, *b),
            };
            let intent = Intent::PxpSwap {
                a: a.index(),
                b: b.index(),
            };
            (write, intent, plan, pxp::check_swap(snap))
        }
        Verb::Sub {
            window,
            input,
            write,
        } => {
            let plan = pxp::sub_source_plan(sub_word()?, *window, *input)
                .ok_or_else(|| String::from("the main window's source is 0x60"))?;
            let intent = Intent::SetSubSource {
                window: window.index(),
                input: *input,
            };
            (write, intent, plan, Vec::new())
        }
        Verb::Apply {
            layout,
            main,
            sub1,
            sub2,
            sub3,
            write,
        } => {
            let main = main
                .or(snap.main_input())
                .ok_or_else(|| String::from("couldn't read the main input (0x60); pass --main"))?;
            let subs: Vec<(Window, u8)> = [
                (Window::Sub1, sub1),
                (Window::Sub2, sub2),
                (Window::Sub3, sub3),
            ]
            .into_iter()
            .filter_map(|(w, i)| i.map(|i| (w, i)))
            .collect();
            let plan = pxp::apply_plan(*layout, main, sub_word()?, &subs);
            let extra = pxp::check_apply(snap, *layout, main, &subs);
            (write, Intent::SetLayout { layout: *layout }, plan, extra)
        }
        Verb::Inset { write, .. } => {
            let intent = Intent::Raw {
                vcp: Vcp::PIP_MODE,
                value: pxp::INSET_STEP,
            };
            (
                write,
                intent,
                pxp::cycle_inset_plan(),
                pxp::check_inset_cycle(snap),
            )
        }
        Verb::Zoom(write) => (write, Intent::Zoom, pxp::zoom_plan(), Vec::new()),
        Verb::Underscan(write) => (write, Intent::Underscan, pxp::underscan_plan(), Vec::new()),
    }))
}

/// What a verb can't confirm by reading back.
fn follow_up(verb: &Verb) -> Option<String> {
    match *verb {
        Verb::Inset {
            from: Some(start), ..
        } => {
            let mut t = pxp::InsetTracker::resumed(start, 0);
            t.step();
            Some(format!("the inset is probably now at {}", t.corner()))
        }
        Verb::Inset { from: None, .. } => Some(format!(
            "the inset moved one corner. {}",
            pxp::CORNER_NOT_READABLE
        )),
        Verb::Swap { .. } => Some(String::from("0xE5 doesn't read back, so check the screen")),
        Verb::Zoom(_) | Verb::Underscan(_) => Some(String::from(
            "0xE5 doesn't read back; on the U4323QE this does nothing visible",
        )),
        _ => None,
    }
}

fn show_layouts(ctx: &Ctx) -> u8 {
    let panel = vcp::default_panel();
    let all = pxp::all_layouts();
    if ctx.json {
        let items: Vec<Value> = all
            .iter()
            .map(|g| {
                json!({
                    "code": g.canonical,
                    "hex": format!("0x{:02X}", g.canonical),
                    "name": g.name(panel),
                    "windows": g.window_count(),
                    "arrangement": g.arrangement.label(),
                    "pane_count_proven": g.pane_count_proven(),
                })
            })
            .collect();
        print_json(&items);
        return exit::OK;
    }
    println!("  code  name                    win  arrangement");
    for g in &all {
        let unproven = if g.pane_count_proven() {
            ""
        } else {
            "  (pane count unproven)"
        };
        println!(
            "  0x{:02X}  {:<22}  {}    {}{unproven}",
            g.canonical,
            g.name(panel).unwrap_or("?"),
            g.window_count(),
            g.arrangement.label()
        );
    }
    exit::OK
}

fn show_geometry(layout: u8, ctx: &Ctx) -> u8 {
    let panel = vcp::default_panel();
    let g = pxp::geometry(layout);
    let window = |i: u8| Window::from_index(i).map_or("?", |w| w.name());
    if ctx.json {
        let cells: Vec<Value> = g
            .cells
            .iter()
            .map(|c| {
                json!({
                    "window": window(c.window),
                    "overlay": c.overlay,
                    "col": c.col, "row": c.row, "col_span": c.col_span, "row_span": c.row_span,
                })
            })
            .collect();
        print_json(&json!({
            "code": g.canonical,
            "hex": format!("0x{:02X}", g.canonical),
            "name": g.name(panel),
            "arrangement": g.arrangement.label(),
            "grid": [g.grid.0, g.grid.1],
            "windows": g.window_count(),
            "cells": cells,
            "pane_count_proven": g.pane_count_proven(),
            "provenance": g.provenance.label(),
            "note": g.note,
        }));
        return exit::OK;
    }
    println!("{}", g.describe(panel));
    println!("  grid       {}x{}", g.grid.0, g.grid.1);
    for c in g.cells {
        match c.overlay {
            true => println!("  {:<9}  inset overlay", window(c.window)),
            false => println!(
                "  {:<9}  col {} row {} ({}x{})",
                window(c.window),
                c.col,
                c.row,
                c.col_span,
                c.row_span
            ),
        }
    }
    if let Some(n) = g.note {
        println!("  note       {n}");
    }
    exit::OK
}

fn show_status(snap: &Snapshot, ctx: &Ctx) -> u8 {
    let layout = snap.layout_code();
    let g = layout.map(pxp::geometry);
    let subs = snap.sub.map(pxp::sub_sources);
    let name = |code: u8| match code {
        0 => String::from("unassigned"),
        c => vcp::input_name(snap.panel, c),
    };
    // (window, source, place) for each window the layout has.
    let rows: Vec<(&str, String, String)> = g
        .iter()
        .flat_map(|g| g.windows().into_iter().map(move |w| (g, w)))
        .map(|(g, w)| {
            let source = match w.sub_field() {
                None => snap.main_input().map(name),
                Some(f) => subs.map(|s| name(s[f])),
            };
            let place = match g.cell(w) {
                Some(c) if c.overlay => String::from("inset"),
                Some(c) => format!("col {} row {}", c.col, c.row),
                None => String::from("-"),
            };
            (w.name(), source.unwrap_or_else(|| "?".into()), place)
        })
        .collect();
    let busy = snap.status_bits().map(|s| s.busy());
    if ctx.json {
        let windows: Vec<Value> = rows
            .iter()
            .map(|(w, src, place)| json!({ "window": w, "source": src, "place": place }))
            .collect();
        print_json(&json!({
            "layout": layout,
            "arrangement": g.map(|g| g.arrangement.label()),
            "windows": windows,
            "sub_word": snap.sub.map(|w| format!("0x{w:04X}")),
            "busy": busy,
            "swap_offered": layout.map(pxp::ddpm_offers_swap),
            "inset": layout.map(pxp::layout_has_inset),
        }));
        return exit::OK;
    }
    match &g {
        Some(g) => println!("{}", g.describe(snap.panel)),
        None => println!("layout couldn't be read (0xE9)"),
    }
    for (w, src, place) in &rows {
        println!("  {w:<6}  {src:<12}  {place}");
    }
    if busy == Some(true) {
        println!("  busy: the on-screen menu is open, so writes will be dropped");
    }
    exit::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run;
    use ddc_core::vcp::input;
    use ddc_transport::testing::{fast, u4323qe, ReplayI2c};

    fn go(t: ReplayI2c, line: &str) -> (u8, ReplayI2c) {
        let mut d = fast(t);
        let code = run(&mut d, &format!("pxp {line}"));
        (code, d.transport)
    }

    #[test]
    fn swap_sends_the_verified_word() {
        let (code, t) = go(u4323qe(), "swap main sub1");
        assert_eq!(code, exit::OK);
        t.assert_sets(&[(0xE5, 0xF010)]);
        assert!(t.every_frame_doubled());
        assert_eq!(go(u4323qe(), "swap").1.sets(), vec![(0xE5, 0xF010)]);
    }

    #[test]
    fn swap_with_release_adds_the_second_write() {
        let (code, t) = go(u4323qe(), "swap main sub1 --release");
        assert_eq!(code, exit::OK);
        t.assert_sets(&[(0xE5, 0xF010), (0xE5, pxp::SWAP_RELEASE)]);
    }

    #[test]
    fn refusals_write_nothing() {
        for (t, line) in [
            (u4323qe().on_get(0xE9, 0x0000), "swap"),
            (u4323qe(), "swap main sub3"),
            (u4323qe(), "inset"),
            (u4323qe(), "sub sub2 dp2"),
            (u4323qe(), "apply 0x42"),
        ] {
            let (code, t) = go(t, line);
            assert_eq!(code, exit::REFUSED, "{line}");
            assert!(t.sets().is_empty(), "{line}");
        }
    }

    #[test]
    fn a_busy_panel_refuses_every_action() {
        for line in [
            "swap",
            "zoom",
            "underscan",
            "inset",
            "sub sub1 dp2",
            "apply quad",
        ] {
            let (code, t) = go(u4323qe().on_get(0xF2, 0x0080), line);
            assert_eq!(code, exit::REFUSED, "{line}");
            assert!(t.sets().is_empty(), "{line}");
        }
    }

    #[test]
    fn zoom_and_underscan_write_their_own_words() {
        assert_eq!(go(u4323qe(), "zoom").1.sets(), vec![(0xE5, 0x0002)]);
        assert_eq!(go(u4323qe(), "underscan").1.sets(), vec![(0xE5, 0x0003)]);
    }

    #[test]
    fn inset_steps_0xe9_in_pip_layouts() {
        let (code, t) = go(u4323qe().on_get(0xE9, 0x0021), "inset --from right-top");
        assert_eq!(code, exit::OK);
        t.assert_sets(&[(0xE9, 0x0002)]);
    }

    #[test]
    fn sub_changes_one_field_and_reads_it_back() {
        let (code, t) = go(u4323qe().on_get(0xE9, 0x0041), "sub sub2 dp2");
        assert_eq!(code, exit::OK);
        let want = pxp::with_sub(0x6DF1, Window::Sub2, input::DP2).unwrap();
        t.assert_sets(&[(0xE8, want)]);
        assert!(t.gets().iter().filter(|c| **c == 0xE8).count() >= 2);
    }

    #[test]
    fn a_mirrored_source_is_allowed() {
        let (code, t) = go(u4323qe(), "sub sub1 usb-c");
        assert_eq!(code, exit::OK);
        assert!(!t.sets().is_empty());
    }

    #[test]
    fn apply_writes_layout_then_input_then_sources() {
        let (code, t) = go(
            u4323qe(),
            "apply quad --main dp2 --sub1 dp --sub2 hdmi1 --sub3 hdmi2",
        );
        assert_eq!(code, exit::OK);
        let word = pxp::apply_subs(
            0x6DF1,
            &[
                (Window::Sub1, input::DISPLAY_PORT),
                (Window::Sub2, input::HDMI_1),
                (Window::Sub3, input::HDMI_2),
            ],
        );
        t.assert_sets(&[(0xE9, 0x0041), (0x60, 0x0013), (0xE8, word)]);
        assert!(t.every_frame_doubled());
    }

    #[test]
    fn apply_keeps_the_current_main_input() {
        let (code, t) = go(u4323qe(), "apply pip");
        assert_eq!(code, exit::OK);
        assert_eq!(t.sets()[..2], [(0xE9, 0x0021), (0x60, 0x001B)]);
    }

    #[test]
    fn a_dry_run_reads_but_never_writes() {
        let (code, t) = go(u4323qe().on_get(0xE9, 0x0041), "sub sub2 dp2 --dry-run");
        assert_eq!(code, exit::OK, "a skipped verify is not a failure");
        assert!(t.sets().is_empty());
        assert!(!t.gets().is_empty());
    }

    #[test]
    fn an_unreadable_sub_word_is_a_failure_not_a_usage_error() {
        let mut t = ReplayI2c::panel().with_caps(ddc_core::fixture::U4323QE);
        for (c, v) in ddc_core::fixture::IDLE_READS
            .iter()
            .filter(|(c, _)| *c != 0xE8)
        {
            t = t.on_get(*c, *v);
        }
        let (code, t) = go(t, "sub sub1 dp2");
        assert_eq!(code, exit::FAILED);
        assert!(t.sets().is_empty());
    }

    #[test]
    fn status_reads_and_never_writes() {
        let (code, t) = go(u4323qe(), "status --json");
        assert_eq!(code, exit::OK);
        assert!(t.sets().is_empty());
        let (code, _) = go(u4323qe(), "");
        assert_eq!(code, exit::OK);
    }

    #[test]
    fn layouts_and_geometry_issue_no_traffic() {
        for line in ["layouts", "geometry quad", "geometry 0x21 --json"] {
            let (code, t) = go(u4323qe(), line);
            assert_eq!(code, exit::OK, "{line}");
            assert!(t.ops().is_empty(), "{line}");
        }
    }
}
