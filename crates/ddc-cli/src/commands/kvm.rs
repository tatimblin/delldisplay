//! `kvm`: the USB KVM behind 0xE7, plus this host's view of where USB went.
//!
//! Reads one guard snapshot, then either shows it or runs one plan through
//! [`exec`]. 0xE7 is an action register and never reads back where USB went,
//! so `usb`, `toggle` and `ensure` count the hub's devices in this Mac's USB
//! tree instead.

use std::time::Duration;

use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use ddc_core::guard::{self, Intent, Snapshot};
use ddc_core::kvm::{self, Association, Commit, MainOwner};
use ddc_core::vcp::{self, Panel, Vcp};
use ddc_transport::{Ddc, I2c, Runner};
use serde_json::{json, Value};

use super::args::{self, Write};
use super::exec::{self, exit, print_json, Ctx};

#[derive(ClapArgs, Debug)]
#[command(disable_help_subcommand = true)]
#[command(
    after_help = "Heads up: switch, toggle, ensure and commit can move your keyboard and \
                        mouse to another machine. Have a way back before you run them."
)]
pub struct Args {
    #[command(subcommand)]
    pub verb: Option<Verb>,
}

#[derive(Subcommand, Debug)]
pub enum Verb {
    /// Everything the KVM depends on, in one read (default).
    Status,
    /// Which host owns the main window.
    Owner,
    /// The input-to-upstream association map.
    Map,
    /// Store one input-to-upstream association. Doesn't switch anything.
    Associate {
        /// dp, dp2, hdmi1, hdmi2, usb-c or 0xNN.
        #[arg(value_parser = args::input)]
        input: u8,
        /// Upstream slot 0-3, or a port name from `kvm map`.
        slot: String,
        #[command(flatten)]
        write: Write,
    },
    /// Bind USB to a PxP window 1-4. Refused unless the panel advertises it.
    BindWindow {
        /// 1-4.
        window: u8,
        #[command(flatten)]
        write: Write,
    },
    /// Dell's six-write commit: main input, sub1 source, layout and association.
    Commit {
        /// Main input: dp, dp2, hdmi1, hdmi2, usb-c or 0xNN.
        #[arg(value_parser = args::input)]
        input: u8,
        /// Layout to commit; defaults to the current one.
        #[arg(long, value_parser = args::layout)]
        layout: Option<u8>,
        /// Source for sub-window 1; defaults to the current one.
        #[arg(long, value_parser = args::input, value_name = "INPUT")]
        sub1: Option<u8>,
        /// Also store an association, e.g. `dp2:1`.
        #[arg(long, value_name = "INPUT:SLOT")]
        associate: Option<String>,
        #[command(flatten)]
        write: Write,
    },
    /// Move USB to the next upstream (0xE7 = 0xFF00). Reports nothing it can't see.
    Switch(Write),
    /// Which machine holds the USB hub, from this Mac's USB tree (macOS).
    Usb,
    /// Flip USB to the other machine and wait to see it go. Not idempotent (macOS).
    Toggle(Write),
    /// Make sure USB is here, or away. Safe to retry (macOS).
    Ensure {
        #[arg(value_enum, default_value_t = Side::Here)]
        side: Side,
        #[command(flatten)]
        write: Write,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Here,
    Away,
}

pub fn run<T: I2c>(d: &mut Ddc<T>, a: &Args, ctx: &Ctx) -> u8 {
    let verb = a.verb.as_ref().unwrap_or(&Verb::Status);
    if matches!(verb, Verb::Usb) {
        return match hub() {
            Ok((n, here)) => show_usb(n, here, ctx),
            Err(e) => exec::fail(ctx, &e),
        };
    }
    let snap = match Runner::new(d).read_snapshot() {
        Ok(s) => s,
        Err(e) => return exec::fail(ctx, &format!("couldn't read the panel: {e}")),
    };
    match verb {
        Verb::Status => show_status(&snap, ctx),
        Verb::Owner => show_owner(&snap, ctx),
        Verb::Map => show_map(&snap, ctx),
        Verb::Associate { input, slot, write } => {
            let Some(slot) = resolve_slot(slot, &snap) else {
                return exec::usage(
                    ctx,
                    &format!("unknown upstream '{slot}' (0-3, or a port name from `kvm map`)"),
                );
            };
            let extra = kvm::check_associate(&snap, *input, slot);
            let refusals = guard::refusals_of(&extra);
            let word = match Association::from_snapshot(&snap).map(|a| a.with_input(*input, slot)) {
                _ if !refusals.is_empty() => Err(refusals),
                Some(Ok(w)) => Ok(w),
                Some(Err(e)) => Err(vec![e.to_string()]),
                None => Err(vec![String::from("couldn't read 0xE7")]),
            };
            match word {
                Ok(w) => {
                    let intent = Intent::KvmAssociate { slot };
                    let plan = kvm::associate_plan(w);
                    exec::apply(
                        ctx,
                        &mut ctx.runner(d, write),
                        &snap,
                        Some(&intent),
                        &plan,
                        extra,
                    )
                }
                Err(why) => exec::refuse(ctx, "associate", why, write.dry_run),
            }
        }
        Verb::BindWindow { window, write } => {
            let extra = kvm::check_bind_window(&snap, *window);
            match (
                kvm::bind_window_plan(*window),
                kvm::bind_window_value(*window),
            ) {
                (Some(plan), Some(value)) => {
                    let intent = Intent::Raw {
                        vcp: Vcp::USB_KVM,
                        value,
                    };
                    exec::apply(
                        ctx,
                        &mut ctx.runner(d, write),
                        &snap,
                        Some(&intent),
                        &plan,
                        extra,
                    )
                }
                _ => {
                    let why = format!("there's no window {window}; windows are 1-4");
                    exec::refuse(ctx, "bind-window", vec![why], write.dry_run)
                }
            }
        }
        Verb::Commit {
            input,
            layout,
            sub1,
            associate,
            write,
        } => {
            let Some(layout) = layout.or(snap.layout_code()) else {
                return exec::fail(ctx, "couldn't read 0xE9; pass --layout");
            };
            let pair = match associate.as_deref().map(|s| parse_pair(s, &snap)) {
                None => None,
                Some(Ok(p)) => Some(p),
                Some(Err(e)) => return exec::usage(ctx, &e),
            };
            match Commit::build(&snap, *input, *sub1, layout, pair) {
                Ok(c) => {
                    let intent = Intent::SetInput { input: *input };
                    let extra = kvm::check_commit(&snap, &c);
                    exec::apply(
                        ctx,
                        &mut ctx.runner(d, write),
                        &snap,
                        Some(&intent),
                        &c.plan(),
                        extra,
                    )
                }
                Err(e) => exec::refuse(ctx, "commit", vec![e.to_string()], write.dry_run),
            }
        }
        Verb::Switch(write) => {
            let mut runner = ctx.runner(d, write);
            exec::apply(
                ctx,
                &mut runner,
                &snap,
                Some(&Intent::KvmToggle),
                &kvm::toggle_plan(),
                vec![],
            )
        }
        Verb::Toggle(write) => flip(d, &snap, None, write, ctx),
        Verb::Ensure { side, write } => flip(d, &snap, Some(*side == Side::Here), write, ctx),
        Verb::Usb => unreachable!("handled before the snapshot"),
    }
}

/// `INPUT:SLOT` for `commit --associate`.
fn parse_pair(s: &str, snap: &Snapshot) -> Result<(u8, u8), String> {
    let (i, slot) = s
        .split_once(':')
        .ok_or_else(|| format!("--associate wants INPUT:SLOT, e.g. dp2:1, not '{s}'"))?;
    let slot = resolve_slot(slot, snap).ok_or_else(|| format!("unknown upstream '{slot}'"))?;
    Ok((args::input(i)?, slot))
}

/// A slot number 0-3, or a port name looked up in the 0xEE inventory.
fn resolve_slot(arg: &str, snap: &Snapshot) -> Option<u8> {
    match arg.parse::<u8>() {
        Ok(n) => (n <= 3).then_some(n),
        Err(_) => Association::from_snapshot(snap).and_then(|a| a.slot_named(arg)),
    }
}

fn hex4(w: Option<u16>) -> Option<String> {
    w.map(|w| format!("0x{w:04X}"))
}

fn input_name(panel: &Panel, code: u8) -> Option<&'static str> {
    panel.value_name(Vcp::INPUT_SOURCE, code)
}

fn owner_json(panel: &Panel, o: Option<MainOwner>) -> Value {
    o.map_or(Value::Null, |o| {
        json!({
            "tag": o.tag(),
            "owns_main": o.asker_owns_main(),
            "main_input": o.main_input(),
            "main_input_name": o.main_input().and_then(|i| input_name(panel, i)),
            "text": o.describe(panel),
        })
    })
}

fn assoc_json(a: Option<&Association>) -> Value {
    let Some(a) = a else { return Value::Null };
    a.entries
        .iter()
        .map(|e| {
            json!({
                "input": e.input,
                "name": input_name(a.panel, e.input),
                "slot": e.slot,
                "port": e.port,
                "writable": e.writable(),
            })
        })
        .collect()
}

fn show_status(snap: &Snapshot, ctx: &Ctx) -> u8 {
    let v = kvm::view(snap);
    if !ctx.json {
        println!("{v}");
        println!("  (`kvm usb` asks this Mac's USB tree where the hub is; 0xE7 can't say)");
        return exit::OK;
    }
    let windows: Vec<Value> = v
        .windows
        .iter()
        .map(|w| {
            json!({
                "window": w.index,
                "input": w.input,
                "input_name": w.input.and_then(|i| input_name(v.panel, i)),
                "live": w.live,
                "slot": w.slot,
                "port": w.port,
            })
        })
        .collect();
    print_json(&json!({
        "owner": owner_json(v.panel, v.owner),
        "layout": v.layout,
        "layout_name": v.layout.and_then(|l| v.panel.value_name(Vcp::PIP_MODE, l)),
        "windows": windows,
        "e7": hex4(snap.kvm),
        "ee": hex4(snap.ports),
        "assoc": assoc_json(v.assoc.as_ref()),
        "field_order": kvm::FIELD_ORDER.label(),
        "note": kvm::NO_LIVE_UPSTREAM_NOTE,
    }));
    exit::OK
}

fn show_owner(snap: &Snapshot, ctx: &Ctx) -> u8 {
    let owner = snap.input.map(kvm::main_owner);
    if ctx.json {
        print_json(&json!({ "owner": owner_json(snap.panel, owner), "raw": hex4(snap.input) }));
        return exit::OK;
    }
    let Some(o) = owner else {
        return exec::fail(ctx, "couldn't read 0x60");
    };
    println!("{}", o.describe(snap.panel));
    match o.asker_owns_main() {
        Some(true) => println!("this machine can drive the main window directly"),
        Some(false) => {
            println!("changing the main input from here takes the screen off that machine")
        }
        None => println!("ownership isn't clear from this read"),
    }
    exit::OK
}

fn show_map(snap: &Snapshot, ctx: &Ctx) -> u8 {
    let Some(a) = Association::from_snapshot(snap) else {
        return exec::fail(ctx, "couldn't read 0xE7");
    };
    if ctx.json {
        print_json(&json!({
            "e7": hex4(Some(a.word)),
            "ee": hex4(a.inventory),
            "field_order": kvm::FIELD_ORDER.label(),
            "assoc": assoc_json(Some(&a)),
        }));
        return exit::OK;
    }
    print!("{a}");
    let stuck: Vec<String> = a
        .unrepresentable()
        .into_iter()
        .map(|i| vcp::input_label(a.panel, i))
        .collect();
    if !stuck.is_empty() {
        println!("  note: no writable field for {}", stuck.join(", "));
    }
    exit::OK
}

fn show_usb(n: usize, here: bool, ctx: &Ctx) -> u8 {
    if ctx.json {
        print_json(&json!({ "usb_here": here, "hub_devices": n }));
    } else {
        println!(
            "usb {} ({n} hub devices)",
            if here { "here" } else { "away" }
        );
    }
    exit::OK
}

/// `(hub devices, attached here)` from this host's USB tree.
fn hub() -> Result<(usize, bool), String> {
    #[cfg(target_os = "macos")]
    return Ok(ddc_transport::macos::usb_here());
    #[cfg(not(target_os = "macos"))]
    Err(String::from(
        "kvm usb/toggle/ensure read this host's USB tree, which only works on macOS; \
         `kvm switch` is the DDC-only toggle",
    ))
}

/// How often, and how many times, to recount the hub after a toggle.
const USB_POLL: Duration = Duration::from_millis(700);
const USB_TRIES: u32 = 12;

/// Toggle USB, then wait for the hub to move. `want` is `Some(here)` for
/// `ensure`, which does nothing when USB is already on that side.
fn flip<T: I2c>(d: &mut Ddc<T>, snap: &Snapshot, want: Option<bool>, w: &Write, ctx: &Ctx) -> u8 {
    let (before, here) = match hub() {
        Ok(h) => h,
        Err(e) => return exec::fail(ctx, &e),
    };
    if want == Some(here) {
        if ctx.json {
            print_json(
                &json!({ "ok": true, "changed": false, "usb_here": here, "hub_devices": before }),
            );
        } else {
            println!("usb already {}", if here { "here" } else { "away" });
        }
        return exit::OK;
    }
    let mut runner = ctx.runner(d, w);
    let plan = kvm::toggle_plan();
    let (mut report, out) = exec::guarded(
        &mut runner,
        snap,
        Some(&Intent::KvmToggle),
        &plan,
        vec![],
        1,
    );
    if out.is_some_and(|o| o.ok()) && !w.dry_run {
        let pause = ctx.wait(USB_POLL);
        let done = |n: usize| match want {
            Some(side) => (n >= 3) == side,
            None => n != before,
        };
        let (after, _) = wait_until(|| hub().map_or(0, |h| h.0), done, USB_TRIES, pause);
        let here = hub().is_ok_and(|h| h.1);
        report.notes.push(format!(
            "hub devices {before} -> {after}; usb is {}",
            if here { "here" } else { "away" }
        ));
        report.extra.insert("usb_here".into(), here.into());
        report.extra.insert("hub_devices".into(), after.into());
        if want.is_some_and(|side| side != here) {
            report.ok = false;
            report.failure = Some(String::from("usb didn't end up on the requested side"));
        }
    }
    report.print(ctx)
}

/// Poll `count` up to `tries` times, `pause` apart, until `done` holds.
/// Returns the last count and how many polls it took.
fn wait_until(
    mut count: impl FnMut() -> usize,
    done: impl Fn(usize) -> bool,
    tries: u32,
    pause: Duration,
) -> (usize, u32) {
    let mut n = 0;
    for i in 1..=tries {
        std::thread::sleep(pause);
        n = count();
        if done(n) {
            return (n, i);
        }
    }
    (n, tries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run;
    use ddc_transport::testing::{fast, u4323qe, Op};

    #[test]
    fn views_read_and_never_write() {
        let mut d = fast(u4323qe());
        for line in [
            "kvm",
            "kvm status --json",
            "kvm owner --json",
            "kvm map --json",
            "kvm map",
        ] {
            assert_eq!(run(&mut d, line), exit::OK, "{line}");
        }
        for code in [0x60u8, 0xE8, 0xE7, 0xEE, 0xE9] {
            assert!(d.transport.gets().contains(&code), "0x{code:02X} not read");
        }
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn associate_reads_0xe7_then_writes_one_field() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "kvm associate dp 3"), exit::OK);
        d.transport.assert_sets(&[(0xE7, 0x2740)]);
        let ops = d.transport.ops();
        let get = ops.iter().position(|o| *o == Op::Get(0xE7)).unwrap();
        let set = ops
            .iter()
            .position(|o| *o == Op::Set(0xE7, 0x2740))
            .unwrap();
        assert!(get < set);
        assert!(d.transport.every_frame_doubled());
    }

    #[test]
    fn associate_takes_a_port_name() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "kvm associate dp usb-c4"), exit::OK);
        d.transport.assert_sets(&[(0xE7, 0x2740)]);
    }

    #[test]
    fn associate_refusals_write_nothing() {
        for (panel, line) in [
            (u4323qe(), "kvm associate hdmi2 1"),
            (u4323qe().on_get(0xF2, 0x0080), "kvm associate dp 3"),
            (u4323qe(), "kvm bind-window 2"),
            (u4323qe(), "kvm bind-window 9"),
        ] {
            let mut d = fast(panel);
            assert_eq!(run(&mut d, line), exit::REFUSED, "{line}");
            assert!(d.transport.sets().is_empty(), "{line}");
        }
    }

    #[test]
    fn bad_upstreams_are_usage_errors() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "kvm associate dp nosuchport"), exit::USAGE);
        assert_eq!(run(&mut d, "kvm commit dp2 --associate dp"), exit::USAGE);
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "kvm associate dp 3 --dry-run"), exit::OK);
        assert_eq!(run(&mut d, "kvm switch --dry-run"), exit::OK);
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn commit_writes_the_wizard_sequence_in_order() {
        let mut d = fast(u4323qe());
        assert_eq!(
            run(&mut d, "kvm commit dp2 --layout 2up-left --associate dp:3"),
            exit::OK
        );
        d.transport.assert_sets(&[
            (0x60, 0x0013),
            (0xE8, 0x6DF1),
            (0xE9, 0x0024),
            (0xE8, 0x6DF1),
            (0xE7, 0x2740),
            (0xE8, 0x6DF1),
        ]);
        assert!(d.transport.every_frame_doubled());
    }

    #[test]
    fn commit_keeps_the_current_layout_by_default() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "kvm commit hdmi1 --settle-ms 0"), exit::OK);
        assert_eq!(d.transport.sets()[2], (0xE9, 0x0024));
    }

    #[test]
    fn switch_writes_the_toggle_word_once() {
        let mut p = u4323qe();
        p.echo_writes = false;
        let mut d = fast(p);
        assert_eq!(run(&mut d, "kvm switch"), exit::OK);
        d.transport.assert_sets(&[(0xE7, 0xFF00)]);
    }

    #[test]
    fn the_usb_wait_stops_as_soon_as_the_hub_moves() {
        let mut seen = [6usize, 6, 18, 18].into_iter();
        let got = wait_until(|| seen.next().unwrap_or(18), |n| n != 6, 12, Duration::ZERO);
        assert_eq!(got, (18, 3));
        assert_eq!(wait_until(|| 6, |n| n != 6, 4, Duration::ZERO), (6, 4));
    }
}
