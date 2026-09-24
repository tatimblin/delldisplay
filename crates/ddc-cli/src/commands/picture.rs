//! `picture`: colour preset, picture mode, volume, mute, power and restores.
//!
//! Each verb reads the capabilities string, 0xF2 and only the codes it needs,
//! then goes through [`exec::guarded`]. Mute works like Dell's app: 0x62 = 0,
//! with the old level remembered in a file on this host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use ddc_core::guard::{Intent, Snapshot, Verdict};
use ddc_core::picture::{
    self, audio, picture_mode, preset, MuteDecision, MuteMemo, Op, Restore, UnmuteDecision,
};
use ddc_core::power::{self, Nap, PowerNapType, PowerState};
use ddc_core::vcp::{self, Panel, Vcp};
use ddc_transport::{Ddc, I2c, Runner};
use serde_json::{json, Value};

use super::args::Write;
use super::exec::{self, exit, print_json, Ctx, Report};

#[derive(ClapArgs, Debug)]
#[command(disable_help_subcommand = true)]
pub struct Args {
    #[command(subcommand)]
    pub verb: Option<Verb>,
}

#[derive(Subcommand, Debug)]
pub enum Verb {
    /// Everything this area can read, decoded (default).
    Status,
    /// Colour preset 0x14. Without NAME, read it.
    Preset {
        /// 5000k, 5700k (warm), 6500k, 7500k, 9300k, 10000k or custom.
        #[arg(value_parser = preset_arg)]
        name: Option<u8>,
        /// Also write picture mode 0xDC first, like Dell's app does.
        #[arg(long)]
        with_mode: bool,
        #[command(flatten)]
        write: Write,
    },
    /// Picture mode 0xDC. Without NAME, read it.
    Mode {
        #[arg(value_parser = mode_arg)]
        name: Option<u8>,
        #[command(flatten)]
        write: Write,
    },
    /// Speaker volume 0x62. Without LEVEL, read it.
    Volume {
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        level: Option<u8>,
        #[command(flatten)]
        write: Write,
    },
    /// Mute like Dell's app: 0x62 = 0, with the old level remembered on this host.
    Mute {
        #[arg(value_enum, default_value_t = MuteAction::Status)]
        action: MuteAction,
        #[command(flatten)]
        write: Write,
    },
    /// Restore levels (0x05), colour (0x08) or factory (0x04) defaults, then read back.
    ///
    /// Needs --i-mean-it. `factory` is DESTRUCTIVE: it resets every OSD setting,
    /// the menu language included, and can't be undone.
    Restore {
        /// levels, colour or factory.
        #[arg(value_parser = scope_arg)]
        scope: Restore,
        /// Yes, really.
        #[arg(long)]
        i_mean_it: bool,
        #[command(flatten)]
        write: Write,
    },
    /// Power state 0xD6: on, standby or off. Without STATE, read it. `on` runs the wake sequence.
    Power {
        #[arg(value_parser = power_arg)]
        state: Option<PowerState>,
        #[command(flatten)]
        write: Write,
    },
    /// Power on and clear PowerNap, retried.
    Wake(Write),
    /// PowerNap 0xE0/0xE1: off, dim or sleep. Without MODE, read it.
    Powernap {
        #[arg(value_parser = nap_arg)]
        mode: Option<Nap>,
        #[command(flatten)]
        write: Write,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuteAction {
    On,
    Off,
    Toggle,
    Status,
    /// Drop the remembered level.
    Forget,
}

// Parsed before the display is open, so names come from the default profile.
fn preset_arg(s: &str) -> Result<u8, String> {
    let p = vcp::default_panel();
    preset::resolve(p, s).ok_or_else(|| format!("try one of: {}", preset::choices(p).join(", ")))
}
fn mode_arg(s: &str) -> Result<u8, String> {
    picture_mode::resolve(vcp::default_panel(), s)
        .ok_or_else(|| String::from("unknown picture mode"))
}
fn scope_arg(s: &str) -> Result<Restore, String> {
    Restore::from_name(s).ok_or_else(|| String::from("expected levels, colour or factory"))
}
fn power_arg(s: &str) -> Result<PowerState, String> {
    PowerState::resolve(s).ok_or_else(|| String::from("expected on, standby or off"))
}
fn nap_arg(s: &str) -> Result<Nap, String> {
    Nap::resolve(s).ok_or_else(|| String::from("expected off, dim or sleep"))
}

/// One read pass: capabilities, 0xF2, and the verb's own codes.
struct Area {
    snap: Snapshot,
    values: BTreeMap<u8, u16>,
    gaps: Vec<u8>,
}

impl Area {
    fn read<T: I2c>(d: &mut Ddc<T>, codes: &[u8]) -> Result<Area, String> {
        let caps = d
            .capabilities()
            .map_err(|e| format!("couldn't read the capabilities string: {e}"))?;
        let mut want = vec![Vcp::STATUS];
        want.extend(codes.iter().filter(|c| caps.supports(**c)));
        let (values, gaps) = Runner::new(d).capture(&want);
        Ok(Area {
            snap: Snapshot::from_reads(d.panel(), caps, &values),
            values: values.into_iter().collect(),
            gaps,
        })
    }
    fn get(&self, code: u8) -> Option<u16> {
        self.values.get(&code).copied()
    }
    fn byte(&self, code: u8) -> Option<u8> {
        self.get(code).map(|v| v as u8)
    }
    /// Both halves of the OSD status, or `None`: the lock check needs both.
    fn osd(&self) -> Option<audio::Osd> {
        Some(audio::Osd {
            v62: self.get(Vcp::VOLUME)?,
            v8d: self.get(picture::AUDIO_MUTE)?,
        })
    }
    fn nap_kind(&self) -> PowerNapType {
        power::powernap_type(&self.snap.caps)
    }
}

pub fn run<T: I2c>(d: &mut Ddc<T>, a: &Args, ctx: &Ctx) -> u8 {
    let verb = a.verb.as_ref().unwrap_or(&Verb::Status);
    if let Verb::Mute {
        action: MuteAction::Forget,
        ..
    } = verb
    {
        clear_memo(&memo_path(ctx));
        if ctx.json {
            print_json(&json!({ "ok": true }));
        } else {
            println!("forgot the stored mute level");
        }
        return exit::OK;
    }
    if let Verb::Restore {
        scope,
        i_mean_it: false,
        write,
    } = verb
    {
        let why = format!(
            "{} Add --i-mean-it if that's what you want.",
            scope.warning()
        );
        return exec::refuse(
            ctx,
            &format!("restore {}", scope.verb()),
            vec![why],
            write.dry_run,
        );
    }
    let codes: Vec<u8> = match verb {
        Verb::Status => STATUS_CODES.to_vec(),
        Verb::Preset { .. } => vec![Vcp::COLOR_PRESET, picture::PICTURE_MODE],
        Verb::Mode { .. } => vec![picture::PICTURE_MODE],
        Verb::Volume { .. } | Verb::Mute { .. } => vec![Vcp::VOLUME, picture::AUDIO_MUTE],
        Verb::Restore { scope, .. } => [&[scope.vcp()], scope.confirms()].concat(),
        Verb::Power { .. } | Verb::Wake(_) | Verb::Powernap { .. } => WAKE_CODES.to_vec(),
    };
    let area = match Area::read(d, &codes) {
        Ok(a) => a,
        Err(e) => return exec::fail(ctx, &e),
    };
    match verb {
        Verb::Status => status(&area, ctx),
        Verb::Preset { name: None, .. } => show_enum(&area, Vcp::COLOR_PRESET, preset::name, ctx),
        Verb::Preset {
            name: Some(v),
            with_mode,
            write,
        } => {
            let op = match with_mode {
                true => Op::PresetWithMode {
                    preset: *v,
                    mode: picture_mode::STANDARD,
                },
                false => Op::Preset(*v),
            };
            apply_op(d, &area, op, write, ctx).0.print(ctx)
        }
        Verb::Mode { name: None, .. } => {
            show_enum(&area, picture::PICTURE_MODE, picture_mode::name, ctx)
        }
        Verb::Mode {
            name: Some(v),
            write,
        } => apply_op(d, &area, Op::Mode(*v), write, ctx).0.print(ctx),
        Verb::Volume { level, write } => match (area.get(Vcp::VOLUME), level) {
            (None, _) => exec::fail(ctx, "couldn't read the volume (0x62)"),
            (Some(raw), None) => {
                let v = audio::volume(raw);
                if ctx.json {
                    print_json(&json!({ "volume": v, "raw": format!("0x{raw:04X}") }));
                } else {
                    println!("volume {v}");
                }
                exit::OK
            }
            (Some(_), Some(l)) => apply_op(d, &area, Op::Volume(*l), write, ctx).0.print(ctx),
        },
        Verb::Mute { action, write } => mute(d, &area, *action, write, ctx),
        Verb::Restore { scope, write, .. } => restore(d, &area, *scope, write, ctx),
        Verb::Power { state: None, .. } => {
            match area.get(Vcp::POWER_MODE).map(power::power_state) {
                None => exec::fail(ctx, "couldn't read 0xD6"),
                Some(p) if ctx.json => {
                    print_json(&json!({ "power": p.name(), "code": p.code() }));
                    exit::OK
                }
                Some(p) => {
                    println!("power {} (0x{:02X})", p.name(), p.code());
                    exit::OK
                }
            }
        }
        // A bare 0xD6 = 1 lets a panel that slept via PowerNap go straight back
        // to sleep, so `on` is the wake sequence.
        Verb::Power {
            state: Some(s),
            write,
        } if s.is_on() => do_wake(d, &area, write, ctx),
        Verb::Power {
            state: Some(s),
            write,
        } => {
            let intent = Intent::Raw {
                vcp: Vcp::POWER_MODE,
                value: s.code() as u16,
            };
            let warn = Verdict::Warn(String::from(
                "a panel switched off over DDC may stop answering; bring it back with `wake`",
            ));
            let mut runner = ctx.runner(d, write);
            exec::apply(
                ctx,
                &mut runner,
                &area.snap,
                Some(&intent),
                &power::power_plan(*s),
                vec![warn],
            )
        }
        Verb::Wake(write) => do_wake(d, &area, write, ctx),
        Verb::Powernap { mode, write } => powernap(d, &area, *mode, write, ctx),
    }
}

/// Top-level `wake`.
pub fn wake<T: I2c>(d: &mut Ddc<T>, w: &Write, ctx: &Ctx) -> u8 {
    match Area::read(d, &WAKE_CODES) {
        Ok(area) => do_wake(d, &area, w, ctx),
        Err(e) => exec::fail(ctx, &e),
    }
}

fn apply_op<T: I2c>(
    d: &mut Ddc<T>,
    area: &Area,
    op: Op,
    w: &Write,
    ctx: &Ctx,
) -> (Report, Option<ddc_core::Outcome>) {
    let extra = picture::preflight(&area.snap, &op, area.osd());
    let mut runner = ctx.runner(d, w);
    exec::guarded(
        &mut runner,
        &area.snap,
        Some(&op.intent()),
        &op.plan(area.snap.panel),
        extra,
        1,
    )
}

const STATUS_CODES: [u8; 9] = [
    Vcp::BRIGHTNESS,
    Vcp::CONTRAST,
    Vcp::COLOR_PRESET,
    picture::PICTURE_MODE,
    Vcp::VOLUME,
    picture::AUDIO_MUTE,
    Vcp::POWER_MODE,
    Vcp::POWERNAP_DIM,
    Vcp::POWERNAP_SLEEP,
];

const WAKE_CODES: [u8; 3] = [Vcp::POWER_MODE, Vcp::POWERNAP_DIM, Vcp::POWERNAP_SLEEP];

fn status(area: &Area, ctx: &Ctx) -> u8 {
    let panel = area.snap.panel;
    let named = |code: u8, name: fn(&Panel, u8) -> Option<&'static str>| {
        area.byte(code).map(|b| {
            let n = name(panel, b).unwrap_or("?");
            (n, format!("{n} (0x{b:02X})"))
        })
    };
    let nap = area.nap_kind();
    let nap_state = power::nap_state(
        nap,
        area.get(Vcp::POWERNAP_DIM),
        area.get(Vcp::POWERNAP_SLEEP),
    );
    let power = area.get(Vcp::POWER_MODE).map(power::power_state);
    let memo = load_memo(&memo_path(ctx)).map(|m| m.level);

    // (json key, json value, text); `None` text prints as `--`.
    let rows: Vec<(&str, Value, Option<String>)> = vec![
        (
            "brightness",
            json!(area.byte(Vcp::BRIGHTNESS)),
            area.byte(Vcp::BRIGHTNESS).map(|v| v.to_string()),
        ),
        (
            "contrast",
            json!(area.byte(Vcp::CONTRAST)),
            area.byte(Vcp::CONTRAST).map(|v| v.to_string()),
        ),
        {
            let p = named(Vcp::COLOR_PRESET, preset::name);
            ("preset", json!(p.as_ref().map(|p| p.0)), p.map(|p| p.1))
        },
        {
            let m = named(picture::PICTURE_MODE, picture_mode::name);
            (
                "picture_mode",
                json!(m.as_ref().map(|m| m.0)),
                m.map(|m| m.1),
            )
        },
        (
            "volume",
            json!(area.get(Vcp::VOLUME).map(audio::volume)),
            area.get(Vcp::VOLUME).map(|v| audio::volume(v).to_string()),
        ),
        {
            let s = area.get(picture::AUDIO_MUTE).map(|v| switch(panel, v));
            ("speaker", json!(s.as_ref().map(|s| &s.0)), s.map(|s| s.1))
        },
        {
            let o = area.osd().map(|o| o.describe());
            ("osd", json!(o), o)
        },
        (
            "power",
            json!(power.map(|p| p.name())),
            power.map(|p| format!("{} (0x{:02X})", p.name(), p.code())),
        ),
        (
            "powernap",
            json!(nap_state.map(|n| n.name())),
            Some(format!(
                "{} ({})",
                nap_state.map_or("unknown", |n| n.name()),
                nap.label()
            )),
        ),
        (
            "stored_mute_level",
            json!(memo),
            memo.map(|m| format!("{m} (`mute off` restores it)")),
        ),
    ];
    let gaps: Vec<String> = area.gaps.iter().map(|c| format!("0x{c:02X}")).collect();
    if ctx.json {
        let mut obj: serde_json::Map<String, Value> = rows
            .into_iter()
            .map(|(k, v, _)| (k.to_string(), v))
            .collect();
        obj.insert("unreadable".into(), json!(gaps));
        print_json(&obj);
    } else {
        for (k, _, text) in rows {
            println!(
                "  {:<18} {}",
                k.replace('_', " "),
                text.as_deref().unwrap_or("--")
            );
        }
        if !gaps.is_empty() {
            println!("  {:<18} {}", "unreadable", gaps.join(" "));
        }
    }
    exit::OK
}

/// 0x8D as the profile names it: `("speaker-on", "speaker on (0x01)")` on
/// the U4323QE, `muted`/`unmuted` on MCCS panels.
fn switch(panel: &Panel, v8d: u16) -> (String, String) {
    match audio::switch(panel, v8d) {
        (f, Some(n)) => (
            n.to_string(),
            format!("{} (0x{f:02X})", n.replace('-', " ")),
        ),
        (f, None) => (format!("0x{f:02X}"), format!("0x{f:02X} (no label)")),
    }
}

/// An enumerated code's current value, and what the panel offers.
fn show_enum(area: &Area, code: u8, name: fn(&Panel, u8) -> Option<&'static str>, ctx: &Ctx) -> u8 {
    let name = |v: u8| name(area.snap.panel, v);
    let Some(v) = area.byte(code) else {
        return exec::fail(ctx, &format!("couldn't read 0x{code:02X}"));
    };
    let offered: Vec<&str> = area
        .snap
        .caps
        .legal_values(code)
        .unwrap_or_default()
        .iter()
        .filter_map(|v| name(*v))
        .collect();
    if ctx.json {
        print_json(
            &json!({ "code": code, "hex": format!("0x{code:02X}"), "value": v, "name": name(v), "offered": offered }),
        );
    } else {
        println!("{} (0x{v:02X})", name(v).unwrap_or("?"));
        println!("offered: {}", offered.join(", "));
    }
    exit::OK
}

fn mute<T: I2c>(d: &mut Ddc<T>, area: &Area, action: MuteAction, w: &Write, ctx: &Ctx) -> u8 {
    let Some(raw) = area.get(Vcp::VOLUME) else {
        return exec::fail(
            ctx,
            "couldn't read the volume (0x62), so the mute state is unknown",
        );
    };
    let path = memo_path(ctx);
    let live = audio::volume(raw);
    let stored = load_memo(&path).map(|m| m.level);
    let want_mute = match action {
        MuteAction::On => true,
        MuteAction::Off => false,
        MuteAction::Toggle => live != 0,
        MuteAction::Forget => unreachable!("handled before reading the panel"),
        MuteAction::Status => {
            let field = area
                .get(picture::AUDIO_MUTE)
                .map(|v| switch(area.snap.panel, v));
            if ctx.json {
                print_json(
                    &json!({ "volume": live, "silent": live == 0, "stored_level": stored, "speaker": field.as_ref().map(|f| &f.0) }),
                );
            } else {
                println!("volume        {live}");
                println!(
                    "stored level  {}",
                    stored.map_or("none".into(), |s| s.to_string())
                );
                println!(
                    "0x8D          {}",
                    field.as_ref().map_or("--", |f| f.1.as_str())
                );
            }
            return exit::OK;
        }
    };
    let quiet = |msg: &str| {
        if ctx.json {
            print_json(&json!({ "ok": true, "changed": false, "note": msg }));
        } else {
            println!("{msg}");
        }
        exit::OK
    };
    if want_mute {
        let MuteDecision::Mute(level) = picture::plan_mute(live) else {
            return quiet("the volume is already 0; muting now would remember 0 as the level");
        };
        // Save first: a DDC write is unacknowledged, so a mute that lands after
        // a failed save would lose the level.
        if !w.dry_run {
            if let Err(e) = save_memo(&path, MuteMemo::new(level)) {
                return exec::fail(
                    ctx,
                    &format!("couldn't save the current level ({e}), so not muting"),
                );
            }
        }
        let (mut report, out) = apply_op(d, area, Op::Mute { level }, w, ctx);
        if out.is_none() && !w.dry_run {
            // Refused: put back whatever memo was there before.
            match stored {
                Some(prev) => drop(save_memo(&path, MuteMemo::new(prev))),
                None => clear_memo(&path),
            }
        }
        if report.ok {
            report
                .notes
                .push(format!("remembered volume {level}; `mute off` restores it"));
        }
        return report.print(ctx);
    }
    match picture::plan_unmute(live, stored) {
        UnmuteDecision::Restore(level) => {
            let (report, _) = apply_op(d, area, Op::Volume(level), w, ctx);
            if report.ok && !w.dry_run {
                clear_memo(&path);
            }
            report.print(ctx)
        }
        UnmuteDecision::AlreadyAudible(v) => quiet(&format!("not muted (volume {v})")),
        UnmuteDecision::Moved { live, stored } => exec::refuse(
            ctx,
            "unmute",
            vec![format!(
                "the volume is {live}, not 0, so someone changed it since the mute. Use \
                 `volume {stored}` to put the old level back, or `mute forget` to drop it"
            )],
            w.dry_run,
        ),
        UnmuteDecision::NothingStored => exec::refuse(
            ctx,
            "unmute",
            vec![String::from(
                "the volume is 0 but no level was remembered (muted at the monitor or by \
                 another tool); set one with `volume <n>`",
            )],
            w.dry_run,
        ),
    }
}

fn restore<T: I2c>(d: &mut Ddc<T>, area: &Area, scope: Restore, w: &Write, ctx: &Ctx) -> u8 {
    let (mut report, out) = apply_op(d, area, Op::Restore(scope), w, ctx);
    if let Some(out) = out.filter(|o| o.ok() && !w.dry_run) {
        let changed = picture::restore_changed(area.snap.panel, &out.snapshot, &out.reads);
        if changed.is_empty() {
            report.notes.push(String::from(
                "nothing moved; it was already at the defaults",
            ));
        }
        for (c, was, now) in changed {
            let name = area.snap.panel.lookup(c).map_or("", |v| v.name);
            report
                .notes
                .push(format!("0x{c:02X} {name} {} -> {}", was & 0xFF, now & 0xFF));
        }
        match picture::levels_undo(&out.snapshot).filter(|_| scope.is_undoable()) {
            Some(undo) => report.notes.push(format!(
                "to put the old levels back: {}",
                exec::one_line(undo.writes())
            )),
            None => report
                .notes
                .push(String::from("this restore can't be undone")),
        }
    }
    report.print(ctx)
}

fn do_wake<T: I2c>(d: &mut Ddc<T>, area: &Area, w: &Write, ctx: &Ctx) -> u8 {
    let kind = area.nap_kind();
    let e0 = area.get(Vcp::POWERNAP_DIM);
    let mut runner = ctx.runner(d, w);
    let (mut report, out) = exec::guarded(
        &mut runner,
        &area.snap,
        Some(&power::wake_intent(kind)),
        &power::wake_plan(kind, e0),
        power::wake_notes(kind, e0),
        power::WAKE_TRIES,
    );
    if let Some(out) = out.filter(|_| !w.dry_run) {
        report.notes.push(String::from(match power::woke(&out) {
            Some(true) => "the panel reports 0xD6 = on",
            Some(false) => "the panel still reports 0xD6 = off",
            None => "0xD6 couldn't be read back, so the wake is unconfirmed",
        }));
    }
    report.print(ctx)
}

fn powernap<T: I2c>(d: &mut Ddc<T>, area: &Area, want: Option<Nap>, w: &Write, ctx: &Ctx) -> u8 {
    let kind = area.nap_kind();
    let e0 = area.get(Vcp::POWERNAP_DIM);
    let now = power::nap_state(kind, e0, area.get(Vcp::POWERNAP_SLEEP));
    let Some(want) = want else {
        if ctx.json {
            print_json(&json!({ "type": kind.label(), "state": now.map(|n| n.name()) }));
        } else {
            println!("{} ({})", now.map_or("unknown", |n| n.name()), kind.label());
        }
        return exit::OK;
    };
    let Some(plan) = power::nap_plan(kind, want, e0) else {
        let why = match kind {
            PowerNapType::Unsupported => "this panel doesn't advertise 0xE0, so it has no PowerNap",
            _ => "0xE0 couldn't be read, and writing this bitfield blind would clear bits we can't see",
        };
        return exec::refuse(
            ctx,
            &format!("powernap {}", want.name()),
            vec![why.into()],
            w.dry_run,
        );
    };
    let (vcp, value) = plan
        .writes()
        .first()
        .copied()
        .unwrap_or((Vcp::POWERNAP_DIM, 0));
    let mut extra = Vec::new();
    if !kind.is_observed_here() {
        extra.push(Verdict::Warn(format!(
            "this panel's PowerNap encoding is {}, inferred from Dell's app and untested",
            kind.label()
        )));
    }
    let mut runner = ctx.runner(d, w);
    exec::apply(
        ctx,
        &mut runner,
        &area.snap,
        Some(&Intent::Raw { vcp, value }),
        &plan,
        extra,
    )
}

/// Where the pre-mute level lives, per display.
fn memo_path(ctx: &Ctx) -> PathBuf {
    let base = ctx.state_dir.clone().unwrap_or_else(|| {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
            .unwrap_or_else(std::env::temp_dir)
            .join("delldisplay")
    });
    base.join(format!("mute-level-{}", ctx.display))
}

fn load_memo(path: &Path) -> Option<MuteMemo> {
    MuteMemo::parse(&std::fs::read_to_string(path).ok()?)
}

fn save_memo(path: &Path, memo: MuteMemo) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, memo.encode())
}

fn clear_memo(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run, run_in, state_dir};
    use ddc_core::fixture::U4323QE;
    use ddc_transport::testing::{fast, ReplayI2c};

    /// The U4323QE with this area's registers seeded.
    fn panel() -> ReplayI2c {
        ReplayI2c::panel()
            .with_caps(U4323QE)
            .on_get(0xF2, 0x0000)
            .on_get(0x10, 0x0036)
            .on_get(0x12, 0x004B)
            .on_get(0x14, 0x0005)
            .on_get(0xDC, 0x0000)
            .on_get(0x62, 0x0032)
            .on_get(0x8D, 0x0001)
            .on_get(0xD6, 0x0001)
            .on_get(0xE0, 0x0000)
            .on_get(0xE1, 0x0000)
    }

    fn memo(state: &str) -> PathBuf {
        state_dir(state).join("mute-level-0")
    }

    #[test]
    fn restore_without_the_flag_is_refused_and_writes_nothing() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture restore levels"), exit::REFUSED);
        assert!(d.transport.writes.is_empty(), "not even a read");
    }

    #[test]
    fn restore_levels_writes_0x05_then_reads_back() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture restore levels --i-mean-it"), exit::OK);
        d.transport.assert_sets(&[(0x05, 0x0001)]);
        let ops = d.transport.code_order();
        let wrote = ops.iter().position(|c| *c == 0x05).unwrap();
        assert!(ops[wrote + 1..].contains(&0x10));
        assert!(ops[wrote + 1..].contains(&0x12));
        assert!(d.transport.every_frame_doubled());
    }

    #[test]
    fn restore_colour_writes_0x08_only() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture restore colour --i-mean-it"), exit::OK);
        d.transport.assert_sets(&[(0x08, 0x0001)]);
    }

    #[test]
    fn a_dry_run_restore_writes_nothing() {
        let mut d = fast(panel());
        assert_eq!(
            run(&mut d, "picture restore factory --i-mean-it --dry-run"),
            exit::OK
        );
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn a_preset_the_panel_does_not_offer_is_refused() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture preset srgb"), exit::REFUSED);
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn preset_names_reach_the_wire() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture preset warm"), exit::OK);
        d.transport.assert_sets(&[(0x14, 0x000B)]);

        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture preset cool --with-mode"), exit::OK);
        d.transport.assert_sets(&[(0xDC, 0x0000), (0x14, 0x0008)]);
    }

    #[test]
    fn a_mode_the_panel_does_not_offer_is_refused() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture mode movie"), exit::REFUSED);
        assert!(d.transport.sets().is_empty());
        assert_eq!(run(&mut d, "picture mode standard"), exit::OK);
        d.transport.assert_sets(&[(0xDC, 0x0000)]);
    }

    #[test]
    fn reads_write_nothing() {
        let mut d = fast(panel());
        for line in [
            "picture",
            "picture status --json",
            "picture preset",
            "picture volume",
            "picture power",
            "picture powernap",
            "picture mute",
        ] {
            assert_eq!(run(&mut d, line), exit::OK, "{line}");
        }
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn mute_then_unmute_round_trips_the_level() {
        clear_memo(&memo("m1"));
        let mut d = fast(panel());
        assert_eq!(run_in(&mut d, "picture mute on", "m1"), exit::OK);
        d.transport.assert_sets(&[(0x62, 0x0000)]);
        assert_eq!(load_memo(&memo("m1")).map(|m| m.level), Some(50));

        let mut d = fast(panel().on_get(0x62, 0x0000));
        assert_eq!(run_in(&mut d, "picture mute off", "m1"), exit::OK);
        d.transport.assert_sets(&[(0x62, 0x0032)]);
        assert_eq!(load_memo(&memo("m1")), None, "the memo is spent");
    }

    #[test]
    fn a_stale_memo_is_refused_not_written_back() {
        save_memo(&memo("m2"), MuteMemo::new(50)).unwrap();
        let mut d = fast(panel().on_get(0x62, 0x0014));
        assert_eq!(run_in(&mut d, "picture mute off", "m2"), exit::REFUSED);
        assert!(d.transport.sets().is_empty());
        assert_eq!(load_memo(&memo("m2")).map(|m| m.level), Some(50));
    }

    #[test]
    fn unmuting_with_no_memo_is_refused() {
        clear_memo(&memo("m3"));
        let mut d = fast(panel().on_get(0x62, 0x0000));
        assert_eq!(run_in(&mut d, "picture mute off", "m3"), exit::REFUSED);
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn muting_a_silent_panel_keeps_the_old_memo() {
        save_memo(&memo("m4"), MuteMemo::new(50)).unwrap();
        let mut d = fast(panel().on_get(0x62, 0x0000));
        assert_eq!(run_in(&mut d, "picture mute on", "m4"), exit::OK);
        assert!(d.transport.sets().is_empty());
        assert_eq!(load_memo(&memo("m4")).map(|m| m.level), Some(50));
    }

    #[test]
    fn a_refused_mute_leaves_no_memo_behind() {
        clear_memo(&memo("m5"));
        let mut d = fast(panel().on_get(0xF2, 0x0080));
        assert_eq!(run_in(&mut d, "picture mute on", "m5"), exit::REFUSED);
        assert_eq!(load_memo(&memo("m5")), None);
    }

    #[test]
    fn volume_writes_and_is_capped_at_100() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture volume 20"), exit::OK);
        d.transport.assert_sets(&[(0x62, 0x0014)]);
        assert_eq!(run(&mut d, "picture volume 101"), exit::USAGE);
    }

    #[test]
    fn a_locked_osd_refuses_an_audio_write() {
        // Bit 15 in both registers is the lock; in one alone it's only a warning.
        let mut d = fast(panel().on_get(0x62, 0x8032).on_get(0x8D, 0x8001));
        assert_eq!(run(&mut d, "picture volume 20"), exit::REFUSED);
        assert!(d.transport.sets().is_empty());

        let mut d = fast(panel().on_get(0x62, 0x8032));
        assert_eq!(run(&mut d, "picture volume 20"), exit::OK);
        d.transport.assert_sets(&[(0x62, 0x0014)]);
    }

    #[test]
    fn wake_clears_both_powernap_flags_then_reads_power_back() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "wake"), exit::OK);
        d.transport
            .assert_sets(&[(0xD6, 0x0001), (0xE0, 0x0000), (0xE1, 0x0000)]);
        let ops = d.transport.code_order();
        let last = ops.iter().rposition(|c| *c == 0xE1).unwrap();
        assert!(ops[last + 1..].contains(&0xD6));
    }

    #[test]
    fn power_on_is_the_wake_sequence() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture power on"), exit::OK);
        assert!(d.transport.sets().iter().any(|(c, _)| *c == 0xE0));
    }

    #[test]
    fn power_off_is_not_read_back() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture power off"), exit::OK);
        d.transport.assert_sets(&[(0xD6, 0x0005)]);
        let ops = d.transport.code_order();
        let wrote = ops.iter().rposition(|c| *c == 0xD6).unwrap();
        assert!(!ops[wrote + 1..].contains(&0xD6));
    }

    #[test]
    fn powernap_writes_both_booleans() {
        let mut d = fast(panel());
        assert_eq!(run(&mut d, "picture powernap dim"), exit::OK);
        d.transport.assert_sets(&[(0xE0, 0x0001), (0xE1, 0x0000)]);
    }

    #[test]
    fn a_busy_panel_refuses_every_write() {
        for line in [
            "picture preset warm",
            "picture restore levels --i-mean-it",
            "wake",
        ] {
            let mut d = fast(panel().on_get(0xF2, 0x0080));
            assert_eq!(run(&mut d, line), exit::REFUSED, "{line}");
            assert!(d.transport.sets().is_empty(), "{line}");
        }
    }
}
