//! The tools, as plain functions over a DDC session.
//!
//! [`state`], [`arrange`] and [`restore`] each read the panel fresh, decide,
//! and at most run one plan through [`exec::guarded`], the same write path as
//! `pxp apply`. They know nothing about MCP: [`super`] opens the display,
//! holds the lock and turns a [`Reply`] into a tool result.

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

/// The view alone, for reading back after a write.
fn read_view<T: I2c>(d: &mut Ddc<T>, caps: &Capabilities) -> Option<View> {
    View::read(&snapshot(d, caps))
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
        "busy": now.snap.status_bits().map(|s| s.busy()),
        "restore": now.state.restore.as_ref().map(|s| json!({ "reason": s.reason, "at": s.at })),
        "policy": {
            "allow_takeover": call.cfg.allow_takeover,
            "cooldown_seconds": call.cfg.cooldown_seconds,
            "cooldown_remaining_seconds": cooldown_left(&now.state, call),
        },
    }))
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
    if target.same(&now.view) {
        return Reply::ok(json!({
            "ok": true, "changed": false, "dry_run": dry_run, "panes": before,
            "note": "the monitor already shows that; nothing was written",
        }));
    }
    let hidden = arrange::hidden_by(&now.view, &target, now.this);
    if !hidden.is_empty() && !call.cfg.allow_takeover {
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
    let mut state = now.state.clone();
    let mut after = target;
    let mut notified = false;
    if report.ok && !dry_run {
        after = read_view(d, &now.snap.caps).unwrap_or(target);
        // Changes stack: restore goes back to before the first one, as long as
        // nobody else changed the monitor in between.
        let first = match &state.restore {
            Some(s) if View::from(s.after).same(&now.view) => s.before,
            _ => Rec::from(now.view),
        };
        state.restore = Some(Saved {
            before: first,
            after: after.into(),
            reason: reason.clone(),
            at: call.now,
        });
        state.last_write = Some(call.now);
        if let Err(e) = call.store.save(&now.key, &state) {
            eprintln!("delldisplay mcp: couldn't save state: {e}");
        }
        notified = call.cfg.notify && (call.notify)(&reason);
    }
    let reply = finish(
        report,
        json!({
            "changed": !dry_run && after != now.view,
            "reason": reason,
            "before": before,
            "after": panes_json(panel, &after, now.this),
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
    let target = View::from(saved.before);
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
    let mut after = target;
    if report.ok && !dry_run {
        after = read_view(d, &now.snap.caps).unwrap_or(target);
        let mut state = now.state.clone();
        state.restore = None;
        if let Err(e) = call.store.save(&now.key, &state) {
            eprintln!("delldisplay mcp: couldn't save state: {e}");
        }
    }
    let reply = finish(
        report,
        json!({
            "restored_from": saved.reason,
            "before": panes_json(panel, &now.view, now.this),
            "after": panes_json(panel, &after, now.this),
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
            }
        }

        fn call(&mut self, tool: &str, args: Value) -> Reply {
            let notes = &self.notes;
            let notify = |m: &str| {
                notes.borrow_mut().push(m.to_string());
                true
            };
            let call = Call {
                cfg: &self.cfg,
                store: &self.store,
                now: self.now,
                max_dwell: Some(Duration::ZERO),
                notify: &notify,
                client: Some(String::from("test")),
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
}
