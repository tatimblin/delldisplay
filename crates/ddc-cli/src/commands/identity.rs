//! `identity`: who this panel is (EDID, firmware), and saving or replaying
//! its VCP state.
//!
//! Import only writes allow-listed codes, only onto the panel the snapshot
//! came from (see [`ddc_core::state`]), and goes through [`exec`] like any
//! other write.

use std::time::Duration;

use clap::{Args as ClapArgs, Subcommand};
use ddc_core::edid::{self, Edid};
use ddc_core::guard;
use ddc_core::identity::{self, Firmware, MonitorKey};
use ddc_core::state::{self, ImportOptions, StateSnapshot};
use ddc_transport::{Ddc, I2c, Runner};
use serde_json::json;

use super::args::{self, Write};
use super::exec::{self, exit, print_json, Ctx};

#[derive(ClapArgs, Debug)]
#[command(disable_help_subcommand = true)]
pub struct Args {
    #[command(subcommand)]
    pub verb: Option<Verb>,
}

#[derive(Subcommand, Debug)]
pub enum Verb {
    /// Model, serial, manufacture date and descriptors from the EDID (default).
    Edid {
        /// Also dump the 128-byte block.
        #[arg(long)]
        raw: bool,
    },
    /// The firmware registers 0xC8, 0xFD and 0xC9, and the version they spell.
    Firmware,
    /// Save the live VCP state as JSON.
    Export {
        /// Write to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
    /// Replay a saved state onto this panel. Only allow-listed codes are written.
    Import {
        /// A file from `identity export`.
        file: String,
        /// Allow a panel of the same model with a different serial.
        #[arg(long)]
        force_serial: bool,
        /// Only these codes, e.g. `0x10,0x12` or `brightness,contrast`.
        #[arg(long, value_delimiter = ',', value_parser = args::code)]
        only: Option<Vec<u8>>,
        #[command(flatten)]
        write: Write,
    },
}

/// What the panel says about itself. Without an EDID the key falls back to the
/// capabilities model, with no serial, so it can't tell two panels apart.
struct Identity {
    edid: Result<Edid, String>,
    caps_model: Option<String>,
    key: MonitorKey,
}

fn read_identity<T: I2c>(d: &mut Ddc<T>) -> Identity {
    let edid = d
        .edid()
        .map_err(|e| e.to_string())
        .and_then(|b| edid::parse(&b).map_err(|e| e.to_string()));
    let caps_model = match &edid {
        Ok(_) => None,
        Err(_) => d.capabilities().ok().and_then(|c| c.model),
    };
    let key = match &edid {
        Ok(e) => MonitorKey::from_edid(e),
        Err(_) => MonitorKey::new(caps_model.clone().unwrap_or_default(), ""),
    };
    Identity {
        edid,
        caps_model,
        key,
    }
}

pub fn run<T: I2c>(d: &mut Ddc<T>, a: &Args, ctx: &Ctx) -> u8 {
    match a.verb.as_ref().unwrap_or(&Verb::Edid { raw: false }) {
        Verb::Edid { raw } => show(d, *raw, ctx),
        Verb::Firmware => firmware(d, ctx),
        Verb::Export { out } => export(d, out.as_deref(), ctx),
        Verb::Import {
            file,
            force_serial,
            only,
            write,
        } => {
            let opts = ImportOptions {
                allow_other_serial: *force_serial,
                only: only.clone(),
                current: Vec::new(),
            };
            import(d, file, opts, write, ctx)
        }
    }
}

fn show<T: I2c>(d: &mut Ddc<T>, raw: bool, ctx: &Ctx) -> u8 {
    let id = read_identity(d);
    let e = match &id.edid {
        Ok(e) => e,
        Err(why) => {
            let fallback = match &id.caps_model {
                Some(m) => format!("; the capabilities string says model {m}, but gives no serial"),
                None => String::new(),
            };
            return exec::fail(ctx, &format!("couldn't read the EDID: {why}{fallback}"));
        }
    };
    let hex: String = e.raw.iter().map(|b| format!("{b:02X}")).collect();
    if ctx.json {
        print_json(&json!({
            "name": e.display_name(),
            "manufacturer": e.manufacturer,
            "vendor": e.vendor_name(),
            "model": e.model(),
            "serial": e.serial(),
            "product_code": e.product_code,
            "manufacture": e.manufacture_label(),
            "edid_version": format!("{}.{}", e.version.0, e.version.1),
            "checksum_ok": e.checksum_ok,
            "extension_blocks": e.extensions,
            "descriptors": e.texts(),
            "preferred_mode": e.preferred_timing().map(|t| t.to_string()),
            "key": id.key.to_string(),
            "complete": id.key.is_complete(),
            "raw": raw.then_some(hex),
        }));
        return exit::OK;
    }
    println!("{}", e.display_name());
    println!("  model        {}", e.model());
    println!(
        "  serial       {}",
        e.serial().unwrap_or_else(|| "-- (none published)".into())
    );
    println!("  manufacture  {}", e.manufacture_label());
    println!(
        "  pnp id       {} (product 0x{:04X})",
        e.manufacturer, e.product_code
    );
    let checksum = if e.checksum_ok { "" } else { ", bad checksum" };
    println!("  edid         {}.{}{checksum}", e.version.0, e.version.1);
    if let Some(t) = e.preferred_timing() {
        println!("  preferred    {t}");
    }
    for t in e.texts() {
        println!("  text         {t}");
    }
    let partial = if id.key.is_complete() {
        ""
    } else {
        " (no serial, so not unique)"
    };
    println!("  key          {}{partial}", id.key);
    if raw {
        for (i, chunk) in e.raw.chunks(16).enumerate() {
            let row: Vec<String> = chunk.iter().map(|b| format!("{b:02X}")).collect();
            println!("  {:03}: {}", i * 16, row.join(" "));
        }
    }
    exit::OK
}

/// Read the firmware group in Dell's order and spacing.
fn read_firmware<T: I2c>(d: &mut Ddc<T>, ctx: &Ctx) -> Firmware {
    let gap = ctx.wait(Duration::from_millis(identity::FIRMWARE_GAP_MS));
    let mut reads = Vec::new();
    for (i, code) in identity::FIRMWARE_CODES.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(gap);
        }
        if let Ok(r) = d.get(*code) {
            reads.push((*code, r.current));
        }
    }
    Firmware::from_reads(&reads)
}

fn firmware<T: I2c>(d: &mut Ddc<T>, ctx: &Ctx) -> u8 {
    let fw = read_firmware(d, ctx);
    let controller = fw.controller_vendor().zip(fw.controller);
    if ctx.json {
        let parts: Vec<_> = fw
            .components()
            .iter()
            .map(|c| json!({ "code": c.code, "hex": format!("0x{:02X}", c.code), "label": c.label, "raw": c.raw, "reading": c.reading }))
            .collect();
        print_json(&json!({
            "components": parts,
            "controller": controller.map(|(name, raw)| json!({ "code": raw & 0xFF, "name": name })),
            "version": fw.version(),
        }));
    } else {
        for c in fw.components() {
            println!("  0x{:02X}  {:<20} {}", c.code, c.label, c.reading);
        }
        match controller {
            Some((name, raw)) => println!("  controller   {name} (0x{:02X}, inferred)", raw & 0xFF),
            None => println!("  controller   unknown"),
        }
        match fw.version() {
            Some(v) => println!("  version      {v}"),
            None => println!(
                "  version      unknown: {}",
                identity::FIRMWARE_VERSION_UNKNOWN
            ),
        }
    }
    if fw.is_complete() {
        exit::OK
    } else {
        exit::FAILED
    }
}

fn export<T: I2c>(d: &mut Ddc<T>, out: Option<&str>, ctx: &Ctx) -> u8 {
    let id = read_identity(d);
    let (values, gaps) = Runner::new(d).capture(state::EXPORT_CODES);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.as_secs());
    let snap = StateSnapshot::from_capture(id.key.clone(), now, &values, &gaps);
    if !id.key.is_complete() {
        eprintln!(
            "warning: no serial for '{}', so an import can't confirm it's the same panel",
            id.key.model
        );
    }
    let Some(path) = out else {
        println!("{}", snap.to_json(d.panel()));
        return exit::OK;
    };
    if let Err(e) = std::fs::write(path, snap.to_json(d.panel())) {
        return exec::fail(ctx, &format!("{path}: {e}"));
    }
    if ctx.json {
        print_json(
            &json!({ "wrote": path, "monitor": snap.monitor.to_string(), "codes": snap.values.len(), "gaps": snap.gaps.len() }),
        );
    } else {
        println!(
            "wrote {path} ({} codes, {} didn't answer)",
            snap.values.len(),
            snap.gaps.len()
        );
    }
    exit::OK
}

fn import<T: I2c>(d: &mut Ddc<T>, path: &str, opts: ImportOptions, w: &Write, ctx: &Ctx) -> u8 {
    let snap = match std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|t| StateSnapshot::from_json(&t).map_err(|e| e.to_string()))
    {
        Ok(s) => s,
        Err(e) => return exec::fail(ctx, &format!("{path}: {e}")),
    };
    // Identity first, so a snapshot from another panel is refused having read
    // only the EDID.
    let id = read_identity(d);
    if let Err(why) = state::import_plan(&snap, &id.key, d.panel(), &opts) {
        return exec::refuse(ctx, "import", vec![why], w.dry_run);
    }
    let guard_snap = match Runner::new(d).read_snapshot() {
        Ok(s) => s,
        Err(e) => return exec::fail(ctx, &format!("couldn't read the capabilities string: {e}")),
    };
    // Current values, so codes that already match aren't rewritten.
    let wanted: Vec<u8> = snap
        .values
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| state::IMPORT_ALLOWED.contains(c))
        .collect();
    let (current, _) = Runner::new(d).capture(&wanted);
    let planned = match state::import_plan(
        &snap,
        &id.key,
        d.panel(),
        &ImportOptions { current, ..opts },
    ) {
        Ok(p) => p,
        Err(why) => return exec::refuse(ctx, "import", vec![why], w.dry_run),
    };
    let extra = guard::check_plan(&guard_snap, &planned.plan);
    let mut runner = ctx.runner(d, w);
    let (mut report, _) = exec::guarded(&mut runner, &guard_snap, None, &planned.plan, extra, 1);
    report.warnings.extend(planned.warnings.iter().cloned());
    let skipped: Vec<_> = planned
        .skipped
        .iter()
        .map(|(c, why)| json!({ "code": c, "hex": format!("0x{c:02X}"), "reason": why.reason() }))
        .collect();
    for (c, why) in &planned.skipped {
        report
            .notes
            .push(format!("skipped 0x{c:02X}: {}", why.reason()));
    }
    if planned.is_empty() {
        report
            .notes
            .push(String::from("nothing to write; the panel already matches"));
    }
    report
        .extra
        .insert("monitor_match".into(), planned.matched.label().into());
    report.extra.insert("skipped".into(), skipped.into());
    report.print(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run;
    use ddc_core::fixture::U4323QE;
    use ddc_transport::testing::{edid_block, fast, EdidPanel, ReplayI2c};
    use std::path::{Path, PathBuf};

    fn registers(t: ReplayI2c) -> ReplayI2c {
        ddc_core::fixture::IDLE_READS
            .iter()
            .fold(t, |t, (c, v)| t.on_get(*c, *v))
            .on_get(0x10, 54)
            .on_get(0x12, 75)
            .on_get(0x62, 30)
            .on_get(0xC8, 0x0005)
            .on_get(0xC9, 0x0001)
            .on_get(0xFD, 0x0074)
    }

    fn panel(serial: &str) -> Ddc<EdidPanel> {
        let ddc = registers(ReplayI2c::panel().with_caps(U4323QE));
        fast(EdidPanel::new(ddc, edid_block("U4323QE", serial)))
    }

    /// Save a snapshot for `key` holding `values`, and return its path.
    fn snapshot(name: &str, key: MonitorKey, values: &[(u8, u16)]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("delldisplay-id-{}-{name}.json", std::process::id()));
        std::fs::write(
            &path,
            StateSnapshot::from_capture(key, 0, values, &[])
                .to_json(ddc_core::vcp::default_panel()),
        )
        .unwrap();
        path
    }

    fn import(d: &mut Ddc<EdidPanel>, path: &Path, flags: &str) -> u8 {
        run(d, &format!("identity import {} {flags}", path.display()))
    }

    #[test]
    fn identity_comes_from_the_edid_with_no_vcp_traffic() {
        let mut d = panel("ABC123");
        let id = read_identity(&mut d);
        assert_eq!(id.key, MonitorKey::new("U4323QE", "ABC123"));
        assert_eq!(d.transport.edid_reads, 1);
        assert!(d.transport.ddc.writes.is_empty());
        assert_eq!(run(&mut d, "identity --json"), exit::OK);
        assert_eq!(run(&mut d, "identity edid --raw"), exit::OK);
    }

    #[test]
    fn a_bad_edid_falls_back_to_the_caps_model_without_a_serial() {
        let ddc = registers(ReplayI2c::panel().with_caps(U4323QE));
        let mut d = fast(EdidPanel::new(ddc, vec![0u8; 128]));
        let id = read_identity(&mut d);
        assert!(id.edid.is_err());
        assert_eq!(id.caps_model.as_deref(), Some("U4323QE"));
        assert!(!id.key.is_complete());
        assert_eq!(run(&mut d, "identity"), exit::FAILED);
    }

    #[test]
    fn firmware_reads_the_group_in_order() {
        let mut d = panel("ABC123");
        assert_eq!(run(&mut d, "identity firmware --json"), exit::OK);
        assert_eq!(d.transport.ddc.gets(), vec![0xC8, 0xFD, 0xC9]);
        assert!(d.transport.ddc.every_frame_doubled());
    }

    #[test]
    fn an_incomplete_firmware_read_exits_1() {
        let ddc = ReplayI2c::panel().on_get(0xC8, 0x0005);
        let mut d = fast(EdidPanel::new(ddc, edid_block("U4323QE", "ABC123")));
        assert_eq!(run(&mut d, "identity firmware"), exit::FAILED);
    }

    #[test]
    fn export_then_import_round_trips_and_writes_only_what_differs() {
        let path =
            std::env::temp_dir().join(format!("delldisplay-id-{}-export.json", std::process::id()));
        let mut d = panel("ABC123");
        assert_eq!(
            run(&mut d, &format!("identity export --out {}", path.display())),
            exit::OK
        );
        assert!(d.transport.ddc.sets().is_empty());
        let snap = StateSnapshot::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(snap.monitor, MonitorKey::new("U4323QE", "ABC123"));
        assert_eq!(snap.value_of(0x10), Some(54));

        let mut d = panel("ABC123");
        assert_eq!(import(&mut d, &path, ""), exit::OK);
        assert!(
            d.transport.ddc.sets().is_empty(),
            "the panel already matches"
        );
    }

    #[test]
    fn import_writes_allowed_codes_in_order() {
        let key = MonitorKey::new("U4323QE", "ABC123");
        let path = snapshot(
            "order",
            key,
            &[(0x10, 30), (0xC8, 5), (0x04, 1), (0xE9, 0x21), (0x60, 0x0F)],
        );
        let mut d = panel("ABC123");
        assert_eq!(import(&mut d, &path, ""), exit::OK);
        d.transport
            .ddc
            .assert_sets(&[(0xE9, 0x0021), (0x60, 0x000F), (0x10, 30)]);
        assert!(d.transport.ddc.every_frame_doubled());
    }

    #[test]
    fn import_refusals_exit_3_and_write_nothing() {
        let cases = [
            (
                "other-serial",
                MonitorKey::new("U4323QE", "OTHER"),
                vec![(0x10, 30)],
                "",
            ),
            (
                "other-model",
                MonitorKey::new("U2723QE", "ABC123"),
                vec![(0x10, 30)],
                "--force-serial",
            ),
            (
                "unadvertised",
                MonitorKey::new("U4323QE", "ABC123"),
                vec![(0x63, 1)],
                "",
            ),
        ];
        for (name, key, values, flags) in cases {
            let path = snapshot(name, key, &values);
            let mut d = panel("ABC123");
            assert_eq!(import(&mut d, &path, flags), exit::REFUSED, "{name}");
            assert!(d.transport.ddc.sets().is_empty(), "{name}");
        }
    }

    #[test]
    fn a_sibling_panel_is_allowed_with_the_override() {
        let path = snapshot(
            "sibling",
            MonitorKey::new("U4323QE", "ZZZ999"),
            &[(0x10, 30)],
        );
        let mut d = panel("ABC123");
        assert_eq!(import(&mut d, &path, "--force-serial"), exit::OK);
        d.transport.ddc.assert_sets(&[(0x10, 30)]);
    }

    #[test]
    fn a_busy_panel_refuses_the_import() {
        let path = snapshot("busy", MonitorKey::new("U4323QE", "ABC123"), &[(0x10, 30)]);
        let ddc = registers(ReplayI2c::panel().with_caps(U4323QE)).on_get(0xF2, 0x0080);
        let mut d = fast(EdidPanel::new(ddc, edid_block("U4323QE", "ABC123")));
        assert_eq!(import(&mut d, &path, ""), exit::REFUSED);
        assert!(d.transport.ddc.sets().is_empty());
    }

    #[test]
    fn a_dry_run_import_writes_nothing() {
        let path = snapshot("dry", MonitorKey::new("U4323QE", "ABC123"), &[(0x10, 30)]);
        let mut d = panel("ABC123");
        assert_eq!(
            import(&mut d, &path, "--dry-run --only brightness"),
            exit::OK
        );
        assert!(d.transport.ddc.sets().is_empty());
    }

    #[test]
    fn a_missing_file_exits_1() {
        let mut d = panel("ABC123");
        assert_eq!(
            run(&mut d, "identity import /nonexistent/snap.json"),
            exit::FAILED
        );
    }
}
