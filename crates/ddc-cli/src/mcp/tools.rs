//! The tools, as plain functions over a DDC session.
//!
//! [`state`], [`arrange`] and [`restore`] each read the panel fresh, decide,
//! and run at most one layout plan through [`exec::guarded`], the same write
//! path as `pxp apply`, then at most one USB toggle through
//! [`kvm::toggle_and_wait`], the same as `kvm ensure`. They know nothing about
//! MCP: [`super`] opens the display, holds the lock and turns a [`Reply`] into
//! a tool result.
//!
//! USB follows the user rather than the layout: the keyboard and mouse move
//! when the computer holding them leaves the screen, or when only this
//! computer is left on it, and otherwise stay put unless the agent asks.

use std::time::Duration;

use ddc_core::arrange::{self, Source, View};
use ddc_core::caps::Capabilities;
use ddc_core::guard::{self, Intent, Snapshot};
use ddc_core::pxp::{self, Window};
use ddc_core::vcp::{self, Panel, Vcp};
use ddc_transport::{Ddc, I2c, Runner};
use serde_json::{json, Map, Value};

use super::config::Config;
use super::store::{Rec, Saved, State, Store};
use crate::commands::exec::{self, Report};
use crate::commands::kvm;

/// Everything a tool call gets besides the display and its arguments.
pub struct Call<'a> {
    pub cfg: &'a Config,
    pub store: &'a Store,
    /// Unix seconds.
    pub now: u64,
    /// Cap on every wait. Tests set zero.
    pub max_dwell: Option<Duration>,
    /// Posts a notification; returns whether it went out.
    pub notify: &'a dyn Fn(&str) -> bool,
    /// The agent's name, for the log.
    pub client: Option<String>,
    /// Where the monitor's USB hub is: `(hub devices, attached here)`.
    pub usb: &'a dyn Fn() -> Result<(usize, bool), String>,
}

/// A tool result. `ok: false` becomes an MCP tool error the model can read.
#[derive(Debug)]
pub struct Reply {
    pub ok: bool,
    pub body: Value,
}

impl Reply {
    fn ok(body: Value) -> Reply {
        Reply { ok: true, body }
    }

    pub fn error(msg: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            body: json!({ "ok": false, "error": msg.into() }),
        }
    }

    /// Decided before any write, so nothing was written.
    fn refused(reasons: Vec<String>) -> Reply {
        Reply {
            ok: false,
            body: json!({ "ok": false, "refused": reasons, "note": "nothing was written" }),
        }
    }
}

/// The panel as read at the start of a call.
struct Now {
    snap: Snapshot,
    view: View,
    this: Option<u8>,
    this_from: &'static str,
    model: String,
    key: String,
    state: State,
}

fn read<T: I2c>(d: &mut Ddc<T>, call: &Call) -> Result<Now, String> {
    let edid = d.edid().ok().and_then(|b| ddc_core::edid::parse(&b).ok());
    let model = edid
        .as_ref()
        .map_or_else(|| String::from("unknown"), |e| e.model());
    let key = match edid.as_ref().and_then(|e| e.serial()) {
        Some(serial) => format!("{model} {serial}"),
        None => model.clone(),
    };
    let mut state = call.store.load(&key);
    let mut dirty = false;
    // The capability string takes about 2 s to read and never changes for a
    // given monitor, so it's read once per monitor and kept.
    let caps = match state.caps.as_deref() {
        Some(raw) if edid.is_some() => Capabilities::parse(raw),
        _ => {
            let caps = d
                .capabilities()
                .map_err(|e| format!("couldn't read the monitor: {e}"))?;
            if edid.is_some() {
                state.caps = Some(caps.raw.clone());
                dirty = true;
            }
            caps
        }
    };
    let mut snap = snapshot(d, &caps);
    // One 0x60 read can catch Auto Select mid-hunt; take the commonest of several.
    if let Ok(r) = d.sample_mode(Vcp::INPUT_SOURCE) {
        snap.put(Vcp::INPUT_SOURCE, r.current);
    }
    let view = View::read(&snap).ok_or_else(|| {
        String::from("couldn't read the layout, main input and sub-sources (0xE9, 0x60, 0xE8)")
    })?;
    let detected = snap.input.and_then(arrange::this_input);
    let (this, this_from) = match (call.cfg.self_input, detected, state.this_input) {
        (Some(i), _, _) => (Some(i), "config"),
        (None, Some(i), _) => (Some(i), "detected"),
        (None, None, Some(i)) => (Some(i), "cached"),
        _ => (None, "unknown"),
    };
    if detected.is_some() && detected != state.this_input {
        state.this_input = detected;
        dirty = true;
    }
    if dirty {
        let _ = call.store.save(&key, &state);
    }
    Ok(Now {
        snap,
        view,
        this,
        this_from,
        model,
        key,
        state,
    })
}

/// The guard snapshot, with capabilities already in hand.
fn snapshot<T: I2c>(d: &mut Ddc<T>, caps: &Capabilities) -> Snapshot {
    let (values, _) = Runner::new(d).capture(&guard::READ_ORDER);
    Snapshot::from_reads(d.panel(), caps.clone(), &values)
}

fn input_name(panel: &Panel, code: u8) -> String {
    match code {
        0 => String::from("none"),
        c => vcp::input_name(panel, c),
    }
}

/// The layouts this panel advertises, by name. A second layout with the same
/// shape gets its code appended.
fn layouts(snap: &Snapshot) -> Vec<(String, u8)> {
    let mut out: Vec<(String, u8)> = Vec::new();
    for g in pxp::all_layouts() {
        let Some(name) = arrange::layout_name(&g) else {
            continue;
        };
        if g.canonical != g.layout || !g.advertised(&snap.caps) {
            continue;
        }
        let name = match out.iter().any(|(n, _)| n == name) {
            true => format!("{name}-0x{:02x}", g.layout),
            false => name.to_string(),
        };
        out.push((name, g.layout));
    }
    out
}

fn layout_label(snap: &Snapshot, code: u8) -> String {
    layouts(snap)
        .into_iter()
        .find(|(_, c)| *c == code)
        .map(|(n, _)| n)
        .or_else(|| arrange::layout_name(&pxp::geometry(code)).map(String::from))
        .unwrap_or_else(|| format!("0x{code:02x}"))
}

fn panes_json(panel: &Panel, v: &View, this: Option<u8>) -> Value {
    v.panes()
        .into_iter()
        .map(|p| {
            json!({
                "position": p.position,
                "input": input_name(panel, p.input),
                "this_computer": this == Some(p.input),
            })
        })
        .collect()
}

fn cooldown_left(state: &State, call: &Call) -> u64 {
    state.last_write.map_or(0, |t| {
        (t + call.cfg.cooldown_seconds).saturating_sub(call.now)
    })
}

fn runner<'a, T: I2c>(d: &'a mut Ddc<T>, call: &Call, dry_run: bool) -> Runner<'a, T> {
    let r = Runner::new(d).dry_run(dry_run);
    match call.max_dwell {
        Some(cap) => r.max_dwell(cap),
        None => r,
    }
}

fn dry_run(args: &Map<String, Value>) -> bool {
    args.get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `display_state`.
pub fn state<T: I2c>(d: &mut Ddc<T>, call: &Call) -> Reply {
    let now = match read(d, call) {
        Ok(n) => n,
        Err(e) => return Reply::error(e),
    };
    let panel = now.snap.panel;
    let here: Vec<String> = now
        .view
        .panes()
        .into_iter()
        .filter(|p| now.this == Some(p.input))
        .map(|p| p.position)
        .collect();
    let layouts: Vec<Value> = layouts(&now.snap)
        .into_iter()
        .map(|(name, code)| {
            let places: Vec<String> = arrange::positions(&pxp::geometry(code))
                .into_iter()
                .map(|(_, p)| p)
                .collect();
            json!({ "name": name, "positions": places })
        })
        .collect();
    let inputs: Vec<String> = now
        .snap
        .caps
        .legal_values(Vcp::INPUT_SOURCE)
        .unwrap_or_default()
        .iter()
        .map(|c| input_name(panel, *c))
        .collect();
    Reply::ok(json!({
        "monitor": {
            "model": now.model,
            "profile": panel.model,
            "profile_matches": panel.matches(&now.model),
        },
        "this_computer": {
            "input": now.this.map(|i| input_name(panel, i)),
            "source": now.this_from,
            "on_screen": !here.is_empty(),
            "positions": here,
        },
        "layout": layout_label(&now.snap, now.view.layout),
        "panes": panes_json(panel, &now.view, now.this),
        "layouts": layouts,
        "inputs": inputs,
        "usb": side_name(usb_side(&(call.usb)())),
        "busy": now.snap.status_bits().map(|s| s.busy()),
        "restore": now.state.restore.as_ref().map(|s| json!({ "reason": s.reason, "at": s.at })),
        "policy": {
            "allow_takeover": call.cfg.allow_takeover,
            "cooldown_seconds": call.cfg.cooldown_seconds,
            "cooldown_remaining_seconds": cooldown_left(&now.state, call),
            "move_usb": call.cfg.move_usb,
        },
    }))
}

/// Where the user wants the keyboard and mouse, from `display_arrange`'s `usb`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wish {
    /// Follow the user: move only when the screen leaves them behind.
    Auto,
    /// To this computer, because the user will type or click here.
    This,
    /// Off this computer, to whatever else is on screen.
    Away,
    /// Leave them where they are.
    Stay,
}

fn wish(args: &Map<String, Value>) -> Result<Wish, String> {
    match args.get("usb") {
        None | Some(Value::Null) => Ok(Wish::Auto),
        Some(v) => match v.as_str().map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("auto") => Ok(Wish::Auto),
            Some("self") => Ok(Wish::This),
            Some("away") => Ok(Wish::Away),
            Some("stay") => Ok(Wish::Stay),
            _ => Err(format!("`usb` is auto, self, away or stay, not {v}")),
        },
    }
}

/// `Some(true)` when USB is on this computer, `None` when that can't be told.
fn usb_side(h: &Result<(usize, bool), String>) -> Option<bool> {
    h.as_ref().ok().map(|h| h.1)
}

fn side_name(here: Option<bool>) -> &'static str {
    match here {
        Some(true) => "self",
        Some(false) => "away",
        None => "unknown",
    }
}

/// Where USB should end up, and why, in words for the model.
struct UsbMove {
    /// The side to end on, `true` for this computer, held there even if the
    /// monitor moves USB by itself; `None` leaves USB to the monitor.
    want: Option<bool>,
    why: String,
}

impl UsbMove {
    fn keep(here: bool, why: impl Into<String>) -> UsbMove {
        UsbMove {
            want: Some(here),
            why: why.into(),
        }
    }

    fn hands_off(why: impl Into<String>) -> UsbMove {
        UsbMove {
            want: None,
            why: why.into(),
        }
    }
}

/// Where USB should end up for a change from `before` to `after`. `auto`
/// reacts only to what this change does to the screen, never to how the user
/// left it. An `Err` is a refusal: the agent asked for something that would
/// leave the keyboard and mouse on a computer that isn't on screen, or that
/// this server can't do.
fn plan_usb(
    wish: Wish,
    cfg: &Config,
    here: &Result<(usize, bool), String>,
    this: Option<u8>,
    before: &View,
    after: &View,
) -> Result<UsbMove, String> {
    let explicit = matches!(wish, Wish::This | Wish::Away);
    if !cfg.move_usb {
        return match explicit {
            true => Err(String::from(
                "moving the keyboard and mouse is turned off (move_usb = false in delldisplay's \
                 mcp.toml); leave out `usb`",
            )),
            false => Ok(UsbMove::hands_off("left to the monitor: move_usb is off")),
        };
    }
    let here = match (here, this) {
        (Ok(h), Some(_)) => h.1,
        (Err(e), _) if explicit => return Err(format!("can't tell where USB is: {e}")),
        (Err(e), _) => return Ok(UsbMove::hands_off(format!("left to the monitor: {e}"))),
        (_, None) if explicit => {
            return Err(String::from(
                "can't tell which input is this computer, so can't tell where the keyboard \
                 and mouse should go; set self_input in delldisplay's mcp.toml",
            ))
        }
        (_, None) => {
            return Ok(UsbMove::hands_off(
                "left to the monitor: which input is this computer isn't known",
            ))
        }
    };
    let (on, others) = on_screen(after, this);
    let (was_on, had_others) = on_screen(before, this);
    let (to, why) = match wish {
        Wish::This if !on => {
            return Err(String::from(
                "the keyboard and mouse can't come to this computer while it isn't on screen; \
                 give it a pane as well",
            ))
        }
        Wish::Away if !others => {
            return Err(String::from(
                "nothing else would be on screen to take the keyboard and mouse",
            ))
        }
        Wish::This => (true, "moved to this computer, as asked"),
        Wish::Away => (false, "moved off this computer, as asked"),
        Wish::Auto if here && was_on && !on => (
            false,
            "moved off this computer, which left the screen, to what's showing",
        ),
        Wish::Auto if !here && had_others && !others => {
            (true, "moved to this computer, the only one left on screen")
        }
        Wish::Stay => return Ok(UsbMove::keep(here, "kept where they were, as asked")),
        Wish::Auto => {
            return Ok(UsbMove::keep(
                here,
                "kept where they were: the screen still shows the computer holding them",
            ))
        }
    };
    Ok(match to == here {
        true => UsbMove::keep(
            here,
            match here {
                true => "already on this computer",
                false => "already on another computer",
            },
        ),
        false => UsbMove::keep(to, why),
    })
}

/// Whether this computer, and whether anything else, is on screen in `v`.
fn on_screen(v: &View, this: Option<u8>) -> (bool, bool) {
    let shown = v.visible();
    (
        this.is_some_and(|t| shown.contains(&t)),
        shown.iter().any(|i| Some(*i) != this),
    )
}

/// How many times to look for the monitor moving USB by itself after a
/// layout change, [`kvm::USB_POLL`] apart. It moves USB with its main input
/// within about two seconds of the write returning.
const USB_SETTLE_TRIES: u32 = 4;

/// Whether the monitor will switch USB in this view. It only does with two
/// or more windows up (picture-in-picture or a split); at full screen it
/// refuses with an on-screen message and USB follows the main input instead.
fn usb_switchable(v: &View) -> bool {
    v.panes().len() > 1
}

fn usb_pause(call: &Call) -> Duration {
    call.max_dwell
        .map_or(kvm::USB_POLL, |cap| kvm::USB_POLL.min(cap))
}

/// Switch USB to `want` before a change from a picture-in-picture or split
/// to full screen, while the monitor still takes the switch. It keeps USB
/// where it is afterwards unless the main input changes. Returns the
/// toggle's report, when one ran, and a fresh reading.
#[allow(clippy::too_many_arguments)]
fn usb_before_leaving<T: I2c>(
    d: &mut Ddc<T>,
    call: &Call,
    snap: &Snapshot,
    hub: &Result<(usize, bool), String>,
    want: Option<bool>,
    from: &View,
    to: &View,
    dry_run: bool,
) -> (Option<Report>, Result<(usize, bool), String>) {
    match (want, hub) {
        (Some(w), Ok(h)) if w != h.1 && !dry_run && usb_switchable(from) && !usb_switchable(to) => {
            let r = kvm::toggle_and_wait(
                &mut runner(d, call, false),
                snap,
                *h,
                Some(w),
                call.usb,
                usb_pause(call),
            );
            (Some(r), (call.usb)())
        }
        _ => (None, hub.clone()),
    }
}

/// What [`settle_usb`] did.
struct Settled {
    /// The toggle's report, when one ran.
    report: Option<Report>,
    /// Where USB ended up.
    after: Option<bool>,
    /// What the monitor did on its own, when it did something.
    note: Option<&'static str>,
}

/// Bring USB to `want` after a change, from `before`, read before it. The
/// monitor can move USB by itself when its main input changes, so after a
/// layout change (`settle`) this watches for that first, then toggles only
/// if USB isn't where it should be: when nothing moved it, or when the
/// monitor took it somewhere the user didn't need it. It never toggles when
/// `switchable` is false: at full screen the monitor refuses, on screen.
#[allow(clippy::too_many_arguments)]
fn settle_usb<T: I2c>(
    d: &mut Ddc<T>,
    call: &Call,
    snap: &Snapshot,
    before: (usize, bool),
    want: bool,
    settle: bool,
    switchable: bool,
    dry_run: bool,
) -> Settled {
    let pause = usb_pause(call);
    let mut now = before;
    if settle && !dry_run {
        for _ in 0..USB_SETTLE_TRIES {
            std::thread::sleep(pause);
            if let Ok(h) = (call.usb)() {
                now = h;
            }
            if now.1 != before.1 {
                break;
            }
        }
    }
    let drifted = now.1 != before.1;
    if now.1 == want {
        return Settled {
            report: None,
            after: Some(now.1),
            note: drifted.then_some("moved by the monitor itself, along with its main input"),
        };
    }
    if !switchable {
        return Settled {
            report: None,
            after: Some(now.1),
            note: Some(
                "left to the monitor: at full screen it only switches USB with its main input",
            ),
        };
    }
    let r = kvm::toggle_and_wait(
        &mut runner(d, call, dry_run),
        snap,
        now,
        Some(want),
        call.usb,
        pause,
    );
    let after = match dry_run {
        true => Some(now.1),
        false => r.extra.get("usb_here").and_then(Value::as_bool),
    };
    Settled {
        report: Some(r),
        after,
        note: drifted.then_some(
            "the monitor moved them along with its main input, and they were moved back",
        ),
    }
}

/// The `usb` part of a reply. `report` is the toggle's, when one ran.
fn usb_json(
    before: Option<bool>,
    after: Option<bool>,
    why: &str,
    want: Option<bool>,
    report: Option<&Report>,
    dry_run: bool,
) -> Value {
    let mut v = json!({
        "before": side_name(before),
        "after": side_name(after),
        "moved": !dry_run && before.is_some() && after.is_some() && after != before,
        "why": why,
    });
    if dry_run {
        v["would_move_to"] = json!(want
            .filter(|w| Some(*w) != before)
            .map(|h| side_name(Some(h))));
    }
    if let Some(r) = report {
        if r.code() != exec::exit::OK {
            v["error"] = json!(r
                .failure
                .clone()
                .or_else(|| r.refused.first().cloned())
                .unwrap_or_else(|| String::from("the USB toggle didn't go through")));
        }
        v["report"] = serde_json::to_value(r).expect("a report is plain data");
    }
    v
}

fn source(panel: &Panel, s: &str) -> Result<Source, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "self" => Ok(Source::This),
        "current" => Ok(Source::Current),
        other => match vcp::resolve_value(panel, Vcp::INPUT_SOURCE, other) {
            Some(v) if (1..=0x1E).contains(&v) => Ok(Source::Input(v as u8)),
            _ => Err(format!(
                "'{s}' isn't a source: use self, current, or an input from display_state"
            )),
        },
    }
}

/// `display_arrange`.
pub fn arrange<T: I2c>(d: &mut Ddc<T>, call: &Call, args: &Map<String, Value>) -> Reply {
    let dry_run = dry_run(args);
    let reason = match args.get("reason").and_then(Value::as_str).map(str::trim) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => return Reply::error("`reason` is required: one sentence, for the user, saying why"),
    };
    let Some(wanted) = args.get("layout").and_then(Value::as_str) else {
        return Reply::error("`layout` is required; display_state lists them");
    };
    let wish = match wish(args) {
        Ok(w) => w,
        Err(e) => return Reply::error(e),
    };
    let now = match read(d, call) {
        Ok(n) => n,
        Err(e) => return Reply::error(e),
    };
    let panel = now.snap.panel;
    let wait = cooldown_left(&now.state, call);
    if wait > 0 && !dry_run {
        return Reply::refused(vec![format!(
            "the monitor was changed through this server moments ago; wait {wait}s \
             (cooldown_seconds is {})",
            call.cfg.cooldown_seconds
        )]);
    }
    let all = layouts(&now.snap);
    let Some(&(_, code)) = all.iter().find(|(n, _)| n == wanted) else {
        let names: Vec<&str> = all.iter().map(|(n, _)| n.as_str()).collect();
        return Reply::refused(vec![format!(
            "'{wanted}' isn't a layout this monitor has; it has {}",
            names.join(", ")
        )]);
    };
    let mut picks = Vec::new();
    let mut errors = Vec::new();
    match args.get("panes") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            for (at, v) in m {
                match v.as_str().ok_or_else(|| format!("'{at}' needs a string")) {
                    Ok(s) => match source(panel, s) {
                        Ok(src) => picks.push((at.clone(), src)),
                        Err(e) => errors.push(e),
                    },
                    Err(e) => errors.push(e),
                }
            }
        }
        Some(_) => errors.push(String::from("`panes` is an object of position -> source")),
    }
    let arranged = match arrange::resolve(&now.view, code, &picks, now.this) {
        Ok(a) if errors.is_empty() => a,
        Ok(_) => return Reply::refused(errors),
        Err(e) => return Reply::refused(errors.into_iter().chain(e).collect()),
    };
    let target = arranged.view;
    let before = panes_json(panel, &now.view, now.this);
    let hub = (call.usb)();
    let usb_before = usb_side(&hub);
    let usb = match plan_usb(wish, call.cfg, &hub, now.this, &now.view, &target) {
        Ok(u) => u,
        Err(e) => return Reply::refused(vec![e]),
    };
    let moves_view = !target.same(&now.view);
    if !moves_view && usb.want.is_none_or(|w| Some(w) == usb_before) {
        return Reply::ok(json!({
            "ok": true, "changed": false, "dry_run": dry_run, "panes": before,
            "usb": usb_json(usb_before, usb_before, &usb.why, usb.want, None, dry_run),
            "note": "the monitor already shows that; nothing was written",
        }));
    }
    let hidden = arrange::hidden_by(&now.view, &target, now.this);
    if moves_view && !hidden.is_empty() && !call.cfg.allow_takeover {
        let names: Vec<String> = hidden.iter().map(|i| input_name(panel, *i)).collect();
        let reply = Reply::refused(vec![format!(
            "this would take {} off the screen, and the user may be looking at it. Keep \
             what's showing visible somewhere (a split, or picture-in-picture), or ask the \
             user to set allow_takeover = true in delldisplay's mcp.toml",
            names.join(" and ")
        )]);
        log(
            call,
            "display_arrange",
            &reason,
            &reply,
            &now.view,
            None,
            dry_run,
        );
        return reply;
    }

    let (pre, hub_mid) = usb_before_leaving(
        d, call, &now.snap, &hub, usb.want, &now.view, &target, dry_run,
    );
    let mut after = now.view;
    let mut snap = None;
    let video = moves_view.then(|| {
        let plan = pxp::apply_plan(target.layout, target.main, now.view.sub, &arranged.subs);
        let extra = pxp::check_apply(&now.snap, target.layout, target.main, &arranged.subs);
        let intent = Intent::SetLayout {
            layout: target.layout,
        };
        let (report, _) = exec::guarded(
            &mut runner(d, call, dry_run),
            &now.snap,
            Some(&intent),
            &plan,
            extra,
            1,
        );
        after = target;
        if report.ok && !dry_run {
            let s = snapshot(d, &now.snap.caps);
            after = View::read(&s).unwrap_or(target);
            snap = Some(s);
        }
        report
    });
    let video_ok = video.as_ref().is_none_or(|r| r.ok);
    let (usb_report, usb_after, usb_why) = match (usb.want, &hub_mid) {
        (Some(want), Ok(h)) if video_ok => {
            let snap = snap.as_ref().unwrap_or(&now.snap);
            let switchable = usb_switchable(&target) || (dry_run && usb_switchable(&now.view));
            let s = settle_usb(d, call, snap, *h, want, moves_view, switchable, dry_run);
            let why = s.note.map_or_else(|| usb.why.clone(), String::from);
            // A switch made before leaving the split counts if USB got there.
            let pre = pre.filter(|r| r.code() == exec::exit::OK || s.after != Some(want));
            (s.report.or(pre), s.after, why)
        }
        // Left to the monitor: say where it put them.
        (None, _) if moves_view && video_ok && !dry_run => {
            (None, usb_side(&(call.usb)()), usb.why.clone())
        }
        _ => (None, usb_before, usb.why.clone()),
    };
    let wrote = |r: &Option<Report>| r.as_ref().is_some_and(|r| r.world_changed);
    let changed = !dry_run && (wrote(&video) || wrote(&usb_report));
    let mut notified = false;
    if changed {
        let mut state = now.state.clone();
        // Changes stack: restore goes back to before the first one, as long as
        // nobody else changed the monitor in between.
        let (first, first_usb) = match &state.restore {
            Some(s)
                if View::from(s.after).same(&now.view)
                    && (s.usb_after.is_none() || s.usb_after == usb_before) =>
            {
                (s.before, s.usb_before)
            }
            _ => (Rec::from(now.view), usb_before),
        };
        state.restore = Some(Saved {
            before: first,
            after: after.into(),
            reason: reason.clone(),
            at: call.now,
            usb_before: first_usb,
            usb_after,
        });
        state.last_write = Some(call.now);
        if let Err(e) = call.store.save(&now.key, &state) {
            eprintln!("delldisplay mcp: couldn't save state: {e}");
        }
        let note = match (usb_before, usb_after) {
            (Some(false), Some(true)) => {
                format!("{reason} Keyboard and mouse are on this computer.")
            }
            (Some(true), Some(false)) => {
                format!("{reason} Keyboard and mouse moved with the screen.")
            }
            _ => reason.clone(),
        };
        notified = call.cfg.notify && (call.notify)(&note);
    }
    let usb_body = usb_json(
        usb_before,
        usb_after,
        &usb_why,
        usb.want,
        usb_report.as_ref(),
        dry_run,
    );
    let main = combine(video, usb_report)
        .unwrap_or_else(|| nothing_written("display_arrange", dry_run, "nothing was written"));
    let reply = finish(
        main,
        json!({
            "changed": changed,
            "reason": reason,
            "before": before,
            "after": panes_json(panel, &after, now.this),
            "usb": usb_body,
            "notified": notified,
        }),
    );
    log(
        call,
        "display_arrange",
        &reason,
        &reply,
        &now.view,
        Some(&after),
        dry_run,
    );
    reply
}

/// `display_restore`.
pub fn restore<T: I2c>(d: &mut Ddc<T>, call: &Call, args: &Map<String, Value>) -> Reply {
    let dry_run = dry_run(args);
    let now = match read(d, call) {
        Ok(n) => n,
        Err(e) => return Reply::error(e),
    };
    let panel = now.snap.panel;
    let Some(saved) = now.state.restore.clone() else {
        return Reply::refused(vec![String::from(
            "there's nothing to put back: this computer hasn't changed the monitor, or it \
             was already put back",
        )]);
    };
    if !now.view.same(&View::from(saved.after)) {
        let mut reply = Reply::refused(vec![String::from(
            "the monitor has changed since this computer's last change, so putting it back \
             would undo someone else's",
        )]);
        reply.body["panes"] = panes_json(panel, &now.view, now.this);
        return reply;
    }
    let hub = (call.usb)();
    let usb_before = usb_side(&hub);
    let usb = match (saved.usb_before, usb_before) {
        _ if !call.cfg.move_usb => UsbMove::hands_off("left to the monitor: move_usb is off"),
        (Some(_), Some(is)) if saved.usb_after.is_some_and(|a| a != is) => UsbMove::keep(
            is,
            "kept where they are: someone moved the keyboard and mouse since this computer did",
        ),
        (Some(want), Some(is)) if want == is => UsbMove::keep(is, "already where they were"),
        (Some(want), Some(_)) => UsbMove::keep(want, "put back where they were"),
        (Some(_), None) => UsbMove::hands_off(format!(
            "left to the monitor: {}",
            hub.as_ref().err().map_or("", String::as_str)
        )),
        (None, _) => {
            UsbMove::hands_off("left to the monitor: this computer didn't record where they were")
        }
    };
    let target = View::from(saved.before);
    let moves_view = !target.same(&now.view);
    let (pre, hub_mid) = usb_before_leaving(
        d, call, &now.snap, &hub, usb.want, &now.view, &target, dry_run,
    );
    let mut after = now.view;
    let mut snap = None;
    let video = moves_view.then(|| {
        let subs: Vec<(Window, u8)> = target
            .geometry()
            .windows()
            .into_iter()
            .filter(|w| !w.is_main())
            .map(|w| (w, target.source(w)))
            .collect();
        let plan = pxp::apply_plan(target.layout, target.main, now.view.sub, &subs);
        let extra = pxp::check_apply(&now.snap, target.layout, target.main, &subs);
        let intent = Intent::SetLayout {
            layout: target.layout,
        };
        let (report, _) = exec::guarded(
            &mut runner(d, call, dry_run),
            &now.snap,
            Some(&intent),
            &plan,
            extra,
            1,
        );
        after = target;
        if report.ok && !dry_run {
            let s = snapshot(d, &now.snap.caps);
            after = View::read(&s).unwrap_or(target);
            snap = Some(s);
        }
        report
    });
    let video_ok = video.as_ref().is_none_or(|r| r.ok);
    let (usb_report, usb_after, usb_why) = match (usb.want, &hub_mid) {
        (Some(want), Ok(h)) if video_ok => {
            let snap = snap.as_ref().unwrap_or(&now.snap);
            let switchable = usb_switchable(&target) || (dry_run && usb_switchable(&now.view));
            let s = settle_usb(d, call, snap, *h, want, moves_view, switchable, dry_run);
            let why = s.note.map_or_else(|| usb.why.clone(), String::from);
            // A switch made before leaving the split counts if USB got there.
            let pre = pre.filter(|r| r.code() == exec::exit::OK || s.after != Some(want));
            (s.report.or(pre), s.after, why)
        }
        // Left to the monitor: say where it put them.
        (None, _) if moves_view && video_ok && !dry_run => {
            (None, usb_side(&(call.usb)()), usb.why.clone())
        }
        _ => (None, usb_before, usb.why.clone()),
    };
    let all_ok = video_ok && usb_report.as_ref().is_none_or(|r| r.ok);
    if all_ok && !dry_run {
        let mut state = now.state.clone();
        state.restore = None;
        if let Err(e) = call.store.save(&now.key, &state) {
            eprintln!("delldisplay mcp: couldn't save state: {e}");
        }
    }
    let usb_body = usb_json(
        usb_before,
        usb_after,
        &usb_why,
        usb.want,
        usb_report.as_ref(),
        dry_run,
    );
    let main = combine(video, usb_report).unwrap_or_else(|| {
        nothing_written(
            "restore",
            dry_run,
            "the monitor already looked that way; nothing was written",
        )
    });
    let reply = finish(
        main,
        json!({
            "restored_from": saved.reason,
            "before": panes_json(panel, &now.view, now.this),
            "after": panes_json(panel, &after, now.this),
            "usb": usb_body,
        }),
    );
    log(
        call,
        "display_restore",
        &saved.reason,
        &reply,
        &now.view,
        Some(&after),
        dry_run,
    );
    reply
}

/// A report for a call that ended up writing nothing.
fn nothing_written(action: &str, dry_run: bool, note: &str) -> Report {
    Report {
        ok: true,
        action: action.to_string(),
        dry_run,
        undo_complete: true,
        notes: vec![note.to_string()],
        ..Report::default()
    }
}

/// One report for the reply: the layout's, failed if the USB move failed, or
/// the USB move's when the layout didn't change. Each is also in full under
/// the reply's own fields.
fn combine(video: Option<Report>, usb: Option<Report>) -> Option<Report> {
    match (video, usb) {
        (Some(mut v), u) => {
            if let Some(u) = u.filter(|u| u.code() != exec::exit::OK) {
                v.ok = false;
                v.failure = v
                    .failure
                    .or(u.failure)
                    .or_else(|| u.refused.first().cloned());
            }
            Some(v)
        }
        (None, u) => u,
    }
}

/// The write report with the tool's own fields on top.
fn finish(report: Report, extra: Value) -> Reply {
    let ok = report.code() == exec::exit::OK;
    let mut body = json!({ "ok": ok, "dry_run": report.dry_run });
    if let (Some(b), Value::Object(e)) = (body.as_object_mut(), extra) {
        b.extend(e);
        b.insert(
            String::from("report"),
            serde_json::to_value(&report).expect("a report is plain data"),
        );
    }
    Reply { ok, body }
}

fn log(
    call: &Call,
    tool: &str,
    reason: &str,
    reply: &Reply,
    before: &View,
    after: Option<&View>,
    dry_run: bool,
) {
    call.store.log(&json!({
        "at": call.now,
        "tool": tool,
        "client": call.client,
        "reason": reason,
        "dry_run": dry_run,
        "ok": reply.ok,
        "refused": reply.body.get("refused").or_else(|| reply.body.pointer("/report/refused")),
        "before": Rec::from(*before),
        "after": after.map(|v| Rec::from(*v)),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddc_core::vcp::input::{DISPLAY_PORT as DP1, DP2, HDMI_1, USB_C};
    use ddc_transport::testing::{fast, u4323qe, ReplayI2c};

    struct Rig {
        d: Ddc<ReplayI2c>,
        cfg: Config,
        store: Store,
        now: u64,
        notes: std::cell::RefCell<Vec<String>>,
        /// What the USB tree says, one reading per check: `true` is here.
        /// The last reading repeats.
        usb: std::cell::RefCell<std::collections::VecDeque<bool>>,
    }

    impl Rig {
        /// The U4323QE at rest, reset to one full-screen pane showing `main`,
        /// with the 0x60 high byte saying this computer is on `this`.
        fn new(name: &str, main: u8, this: u8) -> Rig {
            let t = u4323qe()
                .on_get(0xE9, 0x0000)
                .on_get(0x60, (this as u16) << 8 | main as u16);
            let base = crate::test_support::state_dir(&format!("mcp-{name}"));
            let _ = std::fs::remove_dir_all(&base);
            Rig {
                d: fast(t),
                cfg: Config {
                    cooldown_seconds: 0,
                    ..Config::default()
                },
                store: Store::new(&base),
                now: 1_000,
                notes: Default::default(),
                usb: std::cell::RefCell::new([true].into()),
            }
        }

        /// USB reads as each of `readings` in turn, then as the last.
        fn usb(&self, readings: &[bool]) {
            *self.usb.borrow_mut() = readings.iter().copied().collect();
        }

        /// USB reads as `from` through a layout change and its settle,
        /// the monitor leaving it alone, and as `to` once toggled.
        fn usb_toggles(&self, from: bool, to: bool) {
            let mut r = vec![from; 1 + USB_SETTLE_TRIES as usize];
            r.push(to);
            self.usb(&r);
        }

        fn call(&mut self, tool: &str, args: Value) -> Reply {
            let notes = &self.notes;
            let notify = |m: &str| {
                notes.borrow_mut().push(m.to_string());
                true
            };
            let usb = &self.usb;
            let hub = || {
                let mut q = usb.borrow_mut();
                let here = match q.len() {
                    0 | 1 => q.front().copied().unwrap_or(true),
                    _ => q.pop_front().unwrap(),
                };
                Ok((if here { 18 } else { 6 }, here))
            };
            let call = Call {
                cfg: &self.cfg,
                store: &self.store,
                now: self.now,
                max_dwell: Some(Duration::ZERO),
                notify: &notify,
                client: Some(String::from("test")),
                usb: &hub,
            };
            let args = args.as_object().cloned().unwrap_or_default();
            let d = &mut self.d;
            d.transport.clear_writes();
            match tool {
                "state" => state(d, &call),
                "arrange" => arrange(d, &call, &args),
                "restore" => restore(d, &call, &args),
                _ => unreachable!(),
            }
        }

        fn sets(&self) -> Vec<(u8, u16)> {
            self.d.transport.sets()
        }
    }

    const TOGGLE: (u8, u16) = (0xE7, 0xFF00);

    fn split_right() -> Value {
        json!({ "layout": "side-by-side", "panes": { "right": "self" }, "reason": "showing the build" })
    }

    #[test]
    fn the_capability_string_is_read_once_per_monitor() {
        use ddc_transport::testing::{edid_block, EdidPanel, Op};
        let t = EdidPanel::new(u4323qe(), edid_block("DELL U4323QE", "ABC123"));
        let mut d = fast(t);
        let base = crate::test_support::state_dir("mcp-caps-cache");
        let _ = std::fs::remove_dir_all(&base);
        let (cfg, store) = (Config::default(), Store::new(&base));
        let call = Call {
            cfg: &cfg,
            store: &store,
            now: 0,
            max_dwell: Some(Duration::ZERO),
            notify: &|_| false,
            client: None,
            usb: &|| Ok((18, true)),
        };
        let caps_reads = |d: &Ddc<EdidPanel>| {
            d.transport
                .ddc
                .ops()
                .iter()
                .filter(|o| matches!(o, Op::Caps(0)))
                .count()
        };
        assert!(state(&mut d, &call).ok);
        assert_eq!(caps_reads(&d), 1);
        let r = state(&mut d, &call);
        assert!(r.ok);
        assert_eq!(caps_reads(&d), 1, "the second call used the kept string");
        assert_eq!(r.body["layouts"].as_array().unwrap().len(), 11);
        assert!(store.load("DELL U4323QE ABC123").caps.is_some());
    }

    #[test]
    fn state_names_this_computer_and_what_it_can_do() {
        let mut rig = Rig::new("state", DP1, DP2);
        let r = rig.call("state", json!({}));
        assert!(r.ok, "{}", r.body);
        let b = &r.body;
        assert_eq!(b["this_computer"]["input"], "dp2");
        assert_eq!(b["this_computer"]["source"], "detected");
        assert_eq!(b["this_computer"]["on_screen"], false);
        assert_eq!(b["layout"], "full");
        assert_eq!(b["panes"][0]["input"], "dp1");
        assert_eq!(b["layouts"].as_array().unwrap().len(), 11);
        assert_eq!(b["layouts"][3]["name"], "side-by-side");
        assert_eq!(b["layouts"][3]["positions"], json!(["left", "right"]));
        assert_eq!(b["inputs"].as_array().unwrap().len(), 5);
        assert!(rig.sets().is_empty());
    }

    #[test]
    fn a_split_puts_this_computer_beside_the_user() {
        let mut rig = Rig::new("split", DP1, DP2);
        let r = rig.call("arrange", split_right());
        assert!(r.ok, "{}", r.body);
        assert_eq!(rig.sets()[..2], [(0xE9, 0x0024), (0x60, DP1 as u16)]);
        assert_eq!(pxp::sub_sources(rig.sets()[2].1)[0], DP2);
        assert_eq!(r.body["changed"], true);
        assert_eq!(r.body["after"][1]["this_computer"], true);
        assert_eq!(r.body["notified"], true);
        assert_eq!(*rig.notes.borrow(), ["showing the build"]);
    }

    #[test]
    fn a_takeover_is_refused_unless_allowed() {
        let mut rig = Rig::new("takeover", DP1, DP2);
        let take = json!({ "layout": "full", "panes": { "full": "self" }, "reason": "mine now" });
        let r = rig.call("arrange", take.clone());
        assert!(!r.ok);
        assert!(
            r.body["refused"][0].as_str().unwrap().contains("dp1"),
            "{}",
            r.body
        );
        assert!(rig.sets().is_empty());
        rig.cfg.allow_takeover = true;
        let r = rig.call("arrange", take);
        assert!(r.ok, "{}", r.body);
        assert_eq!(rig.sets()[1], (0x60, DP2 as u16));
    }

    #[test]
    fn restore_puts_back_the_view_from_before_the_first_change() {
        let mut rig = Rig::new("restore", DP1, DP2);
        assert!(rig.call("arrange", split_right()).ok);
        let quad =
            json!({ "layout": "quad", "panes": { "bottom-right": "hdmi1" }, "reason": "more" });
        assert!(rig.call("arrange", quad).ok);
        let r = rig.call("restore", json!({}));
        assert!(r.ok, "{}", r.body);
        assert_eq!(rig.sets()[..2], [(0xE9, 0x0000), (0x60, DP1 as u16)]);
        assert_eq!(
            r.body["after"],
            json!([{ "position": "full", "input": "dp1", "this_computer": false }])
        );
        let again = rig.call("restore", json!({}));
        assert!(!again.ok);
        assert!(again.body["refused"][0]
            .as_str()
            .unwrap()
            .contains("nothing to put back"));
    }

    #[test]
    fn restore_wont_undo_someone_elses_change() {
        let mut rig = Rig::new("someone-else", DP1, DP2);
        assert!(rig.call("arrange", split_right()).ok);
        // Another computer, or the OSD, moves the monitor on.
        rig.d.transport =
            std::mem::replace(&mut rig.d.transport, ReplayI2c::panel()).on_get(0xE9, 0x0041);
        let r = rig.call("restore", json!({}));
        assert!(!r.ok);
        assert!(r.body["refused"][0]
            .as_str()
            .unwrap()
            .contains("someone else"));
        assert!(rig.sets().is_empty());
    }

    #[test]
    fn this_computer_is_remembered_when_the_panel_stops_saying() {
        let mut rig = Rig::new("cached", DP1, DP2);
        assert!(rig.call("arrange", split_right()).ok);
        // Our own 0x60 write echoes back with a zero high byte.
        let r = rig.call("state", json!({}));
        assert_eq!(r.body["this_computer"]["source"], "cached");
        assert_eq!(r.body["this_computer"]["positions"], json!(["right"]));
    }

    #[test]
    fn the_cooldown_spaces_out_changes_but_not_dry_runs() {
        let mut rig = Rig::new("cooldown", DP1, DP2);
        rig.cfg.cooldown_seconds = 30;
        assert!(rig.call("arrange", split_right()).ok);
        rig.now += 5;
        let quad = json!({ "layout": "quad", "reason": "more" });
        let r = rig.call("arrange", quad.clone());
        assert!(
            r.body["refused"][0].as_str().unwrap().contains("25s"),
            "{}",
            r.body
        );
        let mut dry = quad.clone();
        dry["dry_run"] = json!(true);
        assert!(rig.call("arrange", dry).ok);
        assert!(rig.sets().is_empty());
        rig.now += 25;
        assert!(rig.call("arrange", quad).ok);
    }

    #[test]
    fn asking_for_what_is_already_there_writes_nothing() {
        let mut rig = Rig::new("same", DP1, DP2);
        let r = rig.call("arrange", json!({ "layout": "full", "reason": "no-op" }));
        assert!(r.ok);
        assert_eq!(r.body["changed"], false);
        assert!(rig.sets().is_empty());
    }

    #[test]
    fn bad_requests_explain_themselves_and_write_nothing() {
        let mut rig = Rig::new("bad", DP1, DP2);
        for (args, says) in [
            (json!({ "layout": "side-by-side" }), "reason"),
            (
                json!({ "layout": "diagonal", "reason": "x" }),
                "side-by-side",
            ),
            (
                json!({ "layout": "side-by-side", "panes": { "centre": "self" }, "reason": "x" }),
                "left, right",
            ),
            (
                json!({ "layout": "side-by-side", "panes": { "right": "vga" }, "reason": "x" }),
                "isn't a source",
            ),
        ] {
            let r = rig.call("arrange", args);
            assert!(!r.ok);
            assert!(r.body.to_string().contains(says), "{says}: {}", r.body);
            assert!(rig.sets().is_empty());
        }
    }

    #[test]
    fn a_busy_panel_refuses_through_the_usual_guard() {
        let mut rig = Rig::new("busy", DP1, DP2);
        rig.d.transport =
            std::mem::replace(&mut rig.d.transport, ReplayI2c::panel()).on_get(0xF2, 0x0080);
        let r = rig.call("arrange", split_right());
        assert!(!r.ok);
        assert!(r.body["report"]["refused"][0]
            .as_str()
            .unwrap()
            .contains("busy"));
        assert!(rig.sets().is_empty());
    }

    #[test]
    fn a_dry_run_says_what_it_would_write() {
        let mut rig = Rig::new("dry", USB_C, USB_C);
        let mut args =
            json!({ "layout": "pip-small", "panes": { "inset": "hdmi1" }, "reason": "peek" });
        args["dry_run"] = json!(true);
        let r = rig.call("arrange", args);
        assert!(r.ok, "{}", r.body);
        assert_eq!(r.body["report"]["would_write"][0]["hex"], "0xE9");
        assert_eq!(r.body["changed"], false);
        assert!(rig.sets().is_empty());
        let _ = HDMI_1;
    }

    #[test]
    fn state_says_where_the_keyboard_and_mouse_are() {
        let mut rig = Rig::new("usb-state", DP1, DP2);
        assert_eq!(rig.call("state", json!({})).body["usb"], "self");
        rig.usb(&[false]);
        let r = rig.call("state", json!({}));
        assert_eq!(r.body["usb"], "away");
        assert_eq!(r.body["policy"]["move_usb"], true);
    }

    #[test]
    fn leaving_a_split_for_full_screen_switches_usb_while_it_still_can() {
        // USB-C on the left, this computer on the right, USB here. Handing the
        // whole screen to USB-C keeps its main input, so the monitor won't
        // move USB itself, and at full screen it won't take the switch.
        let mut rig = Rig::new("hand-over", DP2, DP2);
        let split = json!({ "layout": "side-by-side",
                            "panes": { "left": "usb-c", "right": "self" }, "reason": "x" });
        assert!(rig.call("arrange", split).ok);
        rig.usb(&[true, false]);
        let r = rig.call(
            "arrange",
            json!({ "layout": "full", "panes": { "full": "usb-c" }, "reason": "your turn" }),
        );
        assert!(r.ok, "{}", r.body);
        let sets = rig.sets();
        assert_eq!(sets[0], TOGGLE, "switched before leaving the split");
        assert_eq!(sets[1], (0xE9, 0x0000));
        assert_eq!(sets.iter().filter(|w| **w == TOGGLE).count(), 1);
        assert_eq!(r.body["usb"]["before"], "self");
        assert_eq!(r.body["usb"]["after"], "away");
        assert_eq!(r.body["usb"]["moved"], true);
        assert!(rig.notes.borrow()[1].contains("Keyboard and mouse"));
    }

    #[test]
    fn a_picture_in_picture_to_look_at_leaves_the_keyboard_alone() {
        let mut rig = Rig::new("peek", DP1, DP2);
        rig.usb(&[false]);
        let r = rig.call(
            "arrange",
            json!({ "layout": "pip-small", "panes": { "inset": "self" }, "reason": "the graph" }),
        );
        assert!(r.ok, "{}", r.body);
        assert!(!rig.sets().contains(&TOGGLE));
        assert_eq!(r.body["usb"]["moved"], false);
        assert_eq!(r.body["usb"]["after"], "away");
    }

    #[test]
    fn asking_for_input_brings_the_keyboard_here_and_restore_sends_it_back() {
        let mut rig = Rig::new("ask", USB_C, DP2);
        rig.usb_toggles(false, true);
        let mut ask = split_right();
        ask["usb"] = json!("self");
        let r = rig.call("arrange", ask);
        assert!(r.ok, "{}", r.body);
        assert_eq!(*rig.sets().last().unwrap(), TOGGLE);
        assert_eq!(r.body["usb"]["after"], "self");
        rig.usb(&[true, false]);
        let r = rig.call("restore", json!({}));
        assert!(r.ok, "{}", r.body);
        // Back to full-screen USB-C: switched while the split was still up.
        assert_eq!(
            rig.sets()[..3],
            [TOGGLE, (0xE9, 0x0000), (0x60, USB_C as u16)]
        );
        assert_eq!(r.body["usb"]["after"], "away");
    }

    #[test]
    fn restore_leaves_the_keyboard_if_someone_moved_it_since() {
        let mut rig = Rig::new("usb-moved", DP1, DP2);
        let r = rig.call("arrange", split_right());
        assert_eq!(r.body["usb"]["moved"], false, "{}", r.body);
        // The user sends USB to the other computer by hand.
        rig.usb(&[false]);
        let r = rig.call("restore", json!({}));
        assert!(r.ok, "{}", r.body);
        assert!(!rig.sets().contains(&TOGGLE));
        assert!(r.body["usb"]["why"]
            .as_str()
            .unwrap()
            .contains("someone moved"));
    }

    #[test]
    fn auto_never_rearranges_what_the_user_set_up() {
        // Another computer is on screen but USB is here: the user's choice.
        let mut rig = Rig::new("usb-as-set", DP1, DP2);
        let r = rig.call(
            "arrange",
            json!({ "layout": "pip-small", "panes": { "inset": "hdmi1" }, "reason": "x" }),
        );
        assert!(r.ok, "{}", r.body);
        assert!(!rig.sets().contains(&TOGGLE));
    }

    #[test]
    fn only_the_keyboard_can_move_when_the_picture_is_already_right() {
        let mut rig = Rig::new("usb-only", DP1, DP2);
        rig.usb(&[false]);
        let split =
            json!({ "layout": "side-by-side", "panes": { "right": "self" }, "reason": "x" });
        assert!(rig.call("arrange", split.clone()).ok);
        rig.usb(&[false, true]);
        let mut ask = split;
        ask["usb"] = json!("self");
        let r = rig.call("arrange", ask);
        assert!(r.ok, "{}", r.body);
        assert_eq!(rig.sets(), [TOGGLE]);
        assert_eq!(r.body["changed"], true);
    }

    #[test]
    fn at_full_screen_the_switch_is_never_sent() {
        // The monitor refuses it there, with a message on screen.
        let mut rig = Rig::new("usb-full", DP2, DP2);
        rig.usb(&[false]);
        let r = rig.call(
            "arrange",
            json!({ "layout": "full", "usb": "self", "reason": "sign in, please" }),
        );
        assert!(r.ok, "{}", r.body);
        assert!(rig.sets().is_empty());
        assert_eq!(r.body["usb"]["after"], "away");
        assert!(r.body["usb"]["why"]
            .as_str()
            .unwrap()
            .contains("full screen"));
    }

    #[test]
    fn the_keyboard_is_never_sent_to_a_computer_off_screen() {
        let mut rig = Rig::new("stranded", DP1, DP2);
        rig.usb(&[false]);
        for (args, says) in [
            (
                json!({ "layout": "full", "usb": "self", "reason": "x" }),
                "isn't on screen",
            ),
            (
                json!({ "layout": "full", "panes": { "full": "self" }, "usb": "away", "reason": "x" }),
                "nothing else",
            ),
        ] {
            rig.cfg.allow_takeover = true;
            let r = rig.call("arrange", args);
            assert!(!r.ok);
            assert!(r.body.to_string().contains(says), "{says}: {}", r.body);
            assert!(rig.sets().is_empty());
        }
    }

    #[test]
    fn move_usb_off_leaves_it_and_refuses_explicit_moves() {
        let mut rig = Rig::new("usb-off", DP2, DP2);
        rig.cfg.move_usb = false;
        let give = json!({ "layout": "full", "panes": { "full": "usb-c" }, "reason": "x" });
        let r = rig.call("arrange", give);
        assert!(r.ok, "{}", r.body);
        assert!(!rig.sets().contains(&TOGGLE));
        let mut ask = split_right();
        ask["usb"] = json!("away");
        let r = rig.call("arrange", ask);
        assert!(r.body.to_string().contains("move_usb"), "{}", r.body);
    }

    #[test]
    fn a_keyboard_that_wont_move_fails_the_call() {
        let mut rig = Rig::new("usb-stuck", DP2, DP2);
        let split = json!({ "layout": "side-by-side",
                            "panes": { "left": "usb-c", "right": "self" }, "reason": "x" });
        assert!(rig.call("arrange", split).ok);
        // The hub never leaves this computer.
        let r = rig.call(
            "arrange",
            json!({ "layout": "full", "panes": { "full": "usb-c" }, "reason": "x" }),
        );
        assert!(!r.ok);
        assert!(r.body["usb"]["error"]
            .as_str()
            .unwrap()
            .contains("requested side"));
        assert_eq!(r.body["report"]["ok"], false);
    }

    #[test]
    fn a_dry_run_says_where_the_keyboard_would_go() {
        let mut rig = Rig::new("usb-dry", DP2, DP2);
        let r = rig.call(
            "arrange",
            json!({ "layout": "full", "panes": { "full": "usb-c" }, "reason": "x", "dry_run": true }),
        );
        assert!(r.ok, "{}", r.body);
        assert!(rig.sets().is_empty());
        assert_eq!(r.body["usb"]["would_move_to"], "away");
        assert_eq!(r.body["usb"]["moved"], false);
    }

    #[test]
    fn when_the_monitor_moves_usb_itself_nothing_is_toggled() {
        // Handing the screen over: the monitor sends USB with its main input.
        let mut rig = Rig::new("monitor-follows", DP2, DP2);
        rig.usb(&[true, false]);
        let r = rig.call(
            "arrange",
            json!({ "layout": "full", "panes": { "full": "usb-c" }, "reason": "your turn" }),
        );
        assert!(r.ok, "{}", r.body);
        assert!(
            !rig.sets().contains(&TOGGLE),
            "a toggle would bounce it back"
        );
        assert_eq!(r.body["usb"]["after"], "away");
        assert_eq!(r.body["usb"]["moved"], true);
        assert!(r.body["usb"]["why"]
            .as_str()
            .unwrap()
            .contains("monitor itself"));
    }

    #[test]
    fn a_keyboard_the_monitor_takes_during_a_peek_is_brought_back() {
        // This computer is on screen with USB; the agent shows USB-C big with
        // itself in the corner, and the monitor sends USB to USB-C with it.
        let mut rig = Rig::new("monitor-takes", DP2, DP2);
        rig.usb(&[true, false, true]);
        let r = rig.call(
            "arrange",
            json!({ "layout": "pip-small", "panes": { "main": "usb-c", "inset": "self" },
                    "reason": "the graph" }),
        );
        assert!(r.ok, "{}", r.body);
        assert_eq!(*rig.sets().last().unwrap(), TOGGLE);
        assert_eq!(r.body["usb"]["after"], "self");
        assert_eq!(r.body["usb"]["moved"], false);
        assert!(r.body["usb"]["why"]
            .as_str()
            .unwrap()
            .contains("moved back"));
    }
}
