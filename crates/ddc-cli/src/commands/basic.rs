//! The plain reads and the raw write: list, status, get, set, caps, codes,
//! coverage.
//!
//! Values are shown masked: enumerated codes by their low byte and name,
//! volume without Dell's status bits.

use clap::Args;
use ddc_core::plan::Plan;
use ddc_core::vcp::{self, Panel, Provenance, Vcp};
use ddc_core::{Reply, ValueKind};
use ddc_transport::{Ddc, I2c};
use serde::Serialize;
use serde_json::json;

use super::args::{self, Write};
use super::exec::{self, exit, print_json, Ctx, Report};

#[derive(Args, Debug)]
pub struct Get {
    /// A name from `codes` (brightness, input, ...) or 0xNN.
    #[arg(value_parser = args::code)]
    pub code: u8,
}

#[derive(Args, Debug)]
pub struct Codes {
    /// List this model's profile instead of the default, e.g. "DELL U2723QE".
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Args, Debug)]
pub struct Set {
    /// A name from `codes` (brightness, input, ...) or 0xNN.
    #[arg(value_parser = args::code)]
    pub code: u8,
    /// A number, 0xNN, or a value name like `usb-c` or `standby`.
    pub value: String,
    /// Needed for the factory resets 0x04, 0x05 and 0x08, which can't be undone.
    #[arg(long)]
    pub i_mean_it: bool,
    #[command(flatten)]
    pub write: Write,
}

/// Codes `status` reads.
const STATUS: [u8; 9] = [0x10, 0x12, 0x14, 0x60, 0x62, 0xD6, 0xE9, 0xE7, 0xE2];

/// Factory resets: `set` wants `--i-mean-it` for these.
const FACTORY: [u8; 3] = [0x04, 0x05, 0x08];

/// One reading, the same shape for `get` and each `status` row.
#[derive(Serialize)]
pub struct Reading {
    code: u8,
    hex: String,
    name: &'static str,
    /// Masked; `null` when the read failed.
    value: Option<u16>,
    max: Option<u16>,
    label: Option<&'static str>,
    #[serde(skip)]
    error: Option<String>,
    #[serde(skip)]
    continuous: bool,
}

impl Reading {
    fn line(&self) -> String {
        let head = format!("0x{:02X}  {:<14}", self.code, self.name);
        match (self.value, self.max) {
            (Some(v), Some(max)) if self.continuous => format!("{head} {v} / {max}"),
            (Some(v), _) => format!("{head} {}", shown(v, self.label)),
            _ => format!("{head} --"),
        }
    }
}

/// Codes that are a level, where "value / max" means something. The type
/// byte can't say, since panels like the U4323QE report enums as continuous.
pub fn is_level(code: u8) -> bool {
    matches!(
        code,
        Vcp::BRIGHTNESS
            | Vcp::CONTRAST
            | Vcp::GAIN_RED
            | Vcp::GAIN_GREEN
            | Vcp::GAIN_BLUE
            | Vcp::VOLUME
    )
}

/// A non-level value: hex, plus its name when the profile has one.
pub fn shown(v: u16, label: Option<&str>) -> String {
    let hex = if v > 0xFF {
        format!("0x{v:04X}")
    } else {
        format!("0x{v:02X}")
    };
    match label {
        Some(l) => format!("{hex} ({l})"),
        None => hex,
    }
}

/// The part of a reply that is the value. Enumerated codes repeat the value or
/// carry padding in the high byte, and 0x62 carries status bits there.
pub fn masked(panel: &Panel, code: u8, r: &Reply) -> u16 {
    let low_byte = panel.is_enumerated(code) || r.kind == ValueKind::NonContinuous;
    if low_byte || code == Vcp::VOLUME {
        r.current & 0xFF
    } else {
        r.current
    }
}

/// Read `code`, sampling it when it's the input, which Auto Select can be
/// scanning through.
pub fn read<T: I2c>(d: &mut Ddc<T>, code: u8) -> Reading {
    let r = if code == Vcp::INPUT_SOURCE {
        d.sample_mode(code)
    } else {
        d.get(code)
    };
    let panel = d.panel();
    let value = r.as_ref().ok().map(|r| masked(panel, code, r));
    Reading {
        code,
        hex: format!("0x{code:02X}"),
        name: panel.lookup(code).map(|c| c.name).unwrap_or(""),
        value,
        max: r.as_ref().ok().map(|r| r.max),
        label: value.and_then(|v| panel.value_name(code, v as u8)),
        error: r.err().map(|e| e.to_string()),
        continuous: is_level(code),
    }
}

pub fn list(ctx: &Ctx) -> u8 {
    #[cfg(target_os = "macos")]
    let n = ddc_transport::macos::AvService::count();
    #[cfg(not(target_os = "macos"))]
    let n = 0usize;
    if ctx.json {
        print_json(&json!({ "displays": n }));
    } else if n == 0 {
        println!("no external displays found");
    } else {
        (0..n).for_each(|i| println!("display {i}"));
    }
    exit::OK
}

pub fn status<T: I2c>(d: &mut Ddc<T>, ctx: &Ctx) -> u8 {
    let rows: Vec<Reading> = STATUS.iter().map(|c| read(d, *c)).collect();
    if ctx.json {
        print_json(&rows);
    } else {
        rows.iter().for_each(|r| println!("  {}", r.line()));
    }
    exit::OK
}

pub fn get<T: I2c>(d: &mut Ddc<T>, a: &Get, ctx: &Ctx) -> u8 {
    let r = read(d, a.code);
    if let Some(e) = &r.error {
        return exec::fail(ctx, e);
    }
    if ctx.json {
        print_json(&r);
    } else {
        println!("{}", r.line());
    }
    exit::OK
}

pub fn set<T: I2c>(d: &mut Ddc<T>, a: &Set, ctx: &Ctx) -> u8 {
    let action = format!("set 0x{:02X}", a.code);
    if FACTORY.contains(&a.code) && !a.i_mean_it {
        let why = format!(
            "0x{:02X} is a factory reset and can't be undone; add --i-mean-it if that's what \
             you want",
            a.code
        );
        return exec::refuse(ctx, &action, vec![why], a.write.dry_run);
    }
    let Some(v) = vcp::resolve_value(d.panel(), a.code, &a.value) else {
        return exec::usage(ctx, &format!("bad value for 0x{:02X}: {}", a.code, a.value));
    };
    let plan = Plan::new(action).write(a.code, v);
    let out = ctx.runner(d, &a.write).run(&plan);
    Report::ran(&plan, &out, Vec::new(), a.write.dry_run).print(ctx)
}

pub fn caps<T: I2c>(d: &mut Ddc<T>, ctx: &Ctx) -> u8 {
    let c = match d.capabilities() {
        Ok(c) => c,
        Err(e) => return exec::fail(ctx, &e.to_string()),
    };
    let panel = d.panel();
    let name = |code: u8| panel.lookup(code).map(|v| v.name).unwrap_or("");
    if ctx.json {
        let vcp: Vec<_> = c
            .vcp
            .iter()
            .map(|e| json!({ "code": e.code, "hex": format!("0x{:02X}", e.code), "name": name(e.code), "values": e.values }))
            .collect();
        print_json(&json!({ "model": c.model, "mccs": c.mccs_ver, "vcp": vcp }));
        return exit::OK;
    }
    println!("model     {}", c.model.as_deref().unwrap_or("?"));
    println!("mccs_ver  {}", c.mccs_ver.as_deref().unwrap_or("?"));
    println!("features  {}", c.vcp.len());
    for e in &c.vcp {
        let vals: Vec<String> = e.values.iter().map(|v| format!("0x{v:02X}")).collect();
        println!("  0x{:02X} {:<16} {}", e.code, name(e.code), vals.join(" "));
    }
    exit::OK
}

pub fn codes(a: &Codes, ctx: &Ctx) -> u8 {
    let panel = match &a.model {
        None => vcp::default_panel(),
        Some(m) => match vcp::for_model(m) {
            Some(p) => p,
            None => return exec::usage(ctx, &format!("no profile for {m}")),
        },
    };
    if ctx.json {
        let all: Vec<_> = panel
            .codes
            .iter()
            .map(|v| json!({ "code": v.vcp, "hex": format!("0x{:02X}", v.vcp), "name": v.name, "verified": v.provenance.is_verified(), "note": v.note }))
            .collect();
        print_json(&all);
    } else {
        for v in panel.codes {
            println!(
                "  0x{:02X}  {:<18} [{}] {}",
                v.vcp,
                v.name,
                v.provenance.label(),
                v.note
            );
        }
    }
    exit::OK
}

/// Counts by provenance rather than "has a profile entry": some entries exist
/// only to say the meaning is unknown.
pub fn coverage<T: I2c>(d: &mut Ddc<T>, ctx: &Ctx) -> u8 {
    let caps = match d.capabilities() {
        Ok(c) => c,
        Err(e) => return exec::fail(ctx, &e.to_string()),
    };
    let panel = d.panel();
    let rows: Vec<(u8, &str, Provenance)> = caps
        .vcp
        .iter()
        .map(|e| match panel.lookup(e.code) {
            Some(c) => (e.code, c.name, c.provenance),
            None => (e.code, "", Provenance::Unknown),
        })
        .collect();
    let count = |p: Provenance| rows.iter().filter(|r| r.2 == p).count();
    let kinds = [
        (Provenance::Observed, "[x]", "tested on this panel"),
        (
            Provenance::Inferred,
            "[~]",
            "from Dell's software or captures",
        ),
        (Provenance::Spec, "[s]", "MCCS spec, untested"),
        (Provenance::Unknown, "[ ]", "meaning unknown"),
    ];
    if ctx.json {
        let codes: Vec<_> = rows
            .iter()
            .map(|(c, n, p)| json!({ "code": c, "hex": format!("0x{c:02X}"), "name": n, "provenance": p.label() }))
            .collect();
        let mut out = json!({ "advertised": rows.len(), "codes": codes });
        for (p, _, _) in kinds {
            out[p.label()] = count(p).into();
        }
        print_json(&out);
        return exit::OK;
    }
    println!("panel advertises {} VCP codes", rows.len());
    for (p, mark, what) in kinds {
        println!("  {mark} {:<9} {:>3}  {what}", p.label(), count(p));
    }
    println!();
    for (c, n, p) in &rows {
        let mark = kinds.iter().find(|k| k.0 == *p).map_or("[ ]", |k| k.1);
        println!("  {mark} 0x{c:02X}  {n}");
    }
    exit::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run;
    use ddc_transport::testing::{fast, u4323qe, DeadI2c};

    #[test]
    fn get_masks_enumerated_values_and_names_them() {
        let mut d = fast(u4323qe().on_get(0xE9, 0x2424));
        let r = read(&mut d, 0xE9);
        assert_eq!(r.value, Some(0x24));
        assert!(r.label.is_some());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            (v["code"].as_u64(), v["hex"].as_str()),
            (Some(0xE9), Some("0xE9"))
        );
        assert_eq!(v["value"], 0x24);
        assert!(v["name"].as_str().is_some_and(|n| !n.is_empty()));
    }

    #[test]
    fn only_levels_show_a_max() {
        let mut d = fast(u4323qe().on_get(0x10, 54).on_get(0x60, 0x1313));
        assert!(read(&mut d, 0x10).line().ends_with("54 / 65535"));
        let input = read(&mut d, 0x60).line();
        assert!(input.ends_with("0x13 (dp2)"), "{input}");
        let kvm = read(&mut d, 0xE7).line();
        assert!(kvm.ends_with("0x2540"), "{kvm}");
    }

    #[test]
    fn volume_is_read_without_its_status_bits() {
        let mut d = fast(u4323qe().on_get(0x62, 0xC032));
        assert_eq!(read(&mut d, 0x62).value, Some(0x32));
    }

    #[test]
    fn a_failed_get_exits_1() {
        let mut d = fast(DeadI2c);
        assert_eq!(run(&mut d, "get brightness"), exit::FAILED);
    }

    #[test]
    fn status_reads_and_never_writes() {
        let mut d = fast(u4323qe().on_get(0x10, 54));
        assert_eq!(run(&mut d, "status --json"), exit::OK);
        assert!(d.transport.sets().is_empty());
        for c in STATUS {
            assert!(d.transport.gets().contains(&c), "0x{c:02X} not read");
        }
    }

    #[test]
    fn set_writes_the_resolved_value() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "set input dp2"), exit::OK);
        d.transport.assert_sets(&[(0x60, 0x13)]);
        assert!(d.transport.every_frame_doubled());
    }

    #[test]
    fn set_dry_run_writes_nothing() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "set brightness 30 --dry-run"), exit::OK);
        assert!(d.transport.writes.is_empty());
    }

    #[test]
    fn a_factory_reset_without_the_flag_is_refused() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "set 0x04 1"), exit::REFUSED);
        assert!(d.transport.writes.is_empty());
        assert_eq!(run(&mut d, "set 0x04 --i-mean-it 1"), exit::OK);
        d.transport.assert_sets(&[(0x04, 1)]);
    }

    #[test]
    fn a_value_that_does_not_resolve_is_a_usage_error() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "set input vga"), exit::USAGE);
        assert!(d.transport.writes.is_empty());
    }
}
