//! The shared write path, output rules and exit codes.
//!
//! A writing verb builds an intent, a plan and any verdicts of its own, then
//! calls [`apply`] (or [`guarded`] when it has notes to add): guards first, so
//! a refusal means nothing was written, then the plan, then one [`Report`].
//! Data goes to stdout; warnings, refusals and errors go to stderr. Under
//! `--json` everything is one JSON object on stdout instead.

use std::path::PathBuf;
use std::time::Duration;

use ddc_core::guard::{self, Intent, Snapshot, Verdict};
use ddc_core::plan::{Outcome, Plan, Step};
use ddc_transport::{Ddc, I2c, Runner};
use serde::Serialize;
use serde_json::{Map, Value};

use super::args::Write;

/// Process exit codes.
pub mod exit {
    pub const OK: u8 = 0;
    /// The panel or transport failed. A write may still have landed; see
    /// `world_changed`.
    pub const FAILED: u8 = 1;
    pub const USAGE: u8 = 2;
    /// A guard said no, so nothing was written.
    pub const REFUSED: u8 = 3;
}

/// What every command gets besides its own arguments.
pub struct Ctx {
    pub json: bool,
    /// Which display. The mute memo is kept per display.
    pub display: usize,
    /// Where the mute memo lives. `None` picks the platform default.
    pub state_dir: Option<PathBuf>,
    /// Cap on every wait when `--settle-ms` isn't given. Tests set zero.
    pub max_dwell: Option<Duration>,
}

impl Ctx {
    /// A runner set up for this verb's `--dry-run` and `--settle-ms`.
    pub fn runner<'a, T: I2c>(&self, d: &'a mut Ddc<T>, w: &Write) -> Runner<'a, T> {
        let r = Runner::new(d).dry_run(w.dry_run);
        match w.settle_ms.map(Duration::from_millis).or(self.max_dwell) {
            Some(cap) => r.max_dwell(cap),
            None => r,
        }
    }

    /// `d`, capped like any other wait.
    pub fn wait(&self, d: Duration) -> Duration {
        self.max_dwell.map_or(d, |cap| d.min(cap))
    }
    /// Where state kept between runs lives: `$XDG_STATE_HOME/delldisplay`,
    /// else `~/.local/state/delldisplay`.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
                .unwrap_or_else(std::env::temp_dir)
                .join("delldisplay")
        })
    }
}

pub fn print_json(v: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string(v).expect("output is plain data")
    );
}

/// Something failed outside a plan: a read, a file.
pub fn fail(ctx: &Ctx, msg: &str) -> u8 {
    error(ctx, msg, exit::FAILED)
}

/// A usage problem only visible once the panel has been read.
pub fn usage(ctx: &Ctx, msg: &str) -> u8 {
    error(ctx, msg, exit::USAGE)
}

fn error(ctx: &Ctx, msg: &str, code: u8) -> u8 {
    if ctx.json {
        print_json(&serde_json::json!({ "ok": false, "error": msg }));
    } else {
        eprintln!("{msg}");
    }
    code
}

/// A refusal decided before any plan: nothing was written.
pub fn refuse(ctx: &Ctx, action: &str, reasons: Vec<String>, dry_run: bool) -> u8 {
    Report::refused(action, reasons, dry_run).print(ctx)
}

/// Guard, run and report one plan.
pub fn apply<T: I2c>(
    ctx: &Ctx,
    runner: &mut Runner<'_, T>,
    snap: &Snapshot,
    intent: Option<&Intent>,
    plan: &Plan,
    extra: Vec<Verdict>,
) -> u8 {
    guarded(runner, snap, intent, plan, extra, 1).0.print(ctx)
}

/// Guard and run, retrying up to `tries`, and hand back the report unprinted
/// with the outcome (`None` when refused) so the caller can add notes.
///
/// `extra` holds the verb's own verdicts. `intent` is `None` for a plan that
/// isn't one operation (an import); then only `extra` guards it.
pub fn guarded<T: I2c>(
    runner: &mut Runner<'_, T>,
    snap: &Snapshot,
    intent: Option<&Intent>,
    plan: &Plan,
    extra: Vec<Verdict>,
    tries: u32,
) -> (Report, Option<Outcome>) {
    let dry_run = runner.is_dry_run();
    let refusals = guard::refusals_of(&extra);
    if !refusals.is_empty() {
        return (Report::refused(&plan.name, refusals, dry_run), None);
    }
    let (outcome, warnings) = match intent {
        Some(i) => match runner.apply_with_tries(snap, i, plan, tries) {
            Ok(v) => v,
            Err(r) => return (Report::refused(&plan.name, r, dry_run), None),
        },
        None => (runner.run_until_ok(plan, tries), Vec::new()),
    };
    let mut all = extra;
    all.extend(warnings.into_iter().map(Verdict::Warn));
    let warnings = guard::warnings_of(&guard::tidy(all));
    (
        Report::ran(plan, &outcome, warnings, dry_run),
        Some(outcome),
    )
}

/// One register value in a report.
#[derive(Serialize, Debug, PartialEq)]
pub struct Reg {
    pub code: u8,
    pub hex: String,
    pub value: u16,
}

fn regs(v: impl IntoIterator<Item = (u8, u16)>) -> Vec<Reg> {
    v.into_iter()
        .map(|(code, value)| Reg {
            code,
            hex: format!("0x{code:02X}"),
            value,
        })
        .collect()
}

/// What every writing verb prints, whether it ran, failed or was refused.
///
/// ```json
/// { "ok": true, "action": "swap main<->sub1", "dry_run": false,
///   "refused": [], "warnings": [], "steps_run": 1, "failure": null,
///   "world_changed": true, "wrote": [{"code": 229, "hex": "0xE5", "value": 61456}],
///   "reads": [], "undo": [], "undo_complete": true, "undo_gaps": [], "notes": [] }
/// ```
///
/// - `ok`: the plan ran to the end. False when refused or failed.
/// - `refused`: why a guard said no. Non-empty means nothing was written (exit 3).
/// - `warnings`: things worth knowing; the write went ahead anyway.
/// - `failure`: the step that stopped the plan, with `steps_run` saying how far it got.
/// - `world_changed`: a write went out. DDC writes are unacknowledged, so a
///   failed one may still have landed.
/// - `wrote`: the writes reached. Named `would_write` under `--dry-run`, when
///   nothing was sent.
/// - `reads`: values read back during the plan.
/// - `undo`: writes that put the panel back after a failure; `undo_gaps` are
///   codes that couldn't be captured first, and `undo_complete` is false if any.
/// - `notes`: anything else the verb has to say.
///
/// Some verbs add their own fields (`identity import` adds `monitor_match` and
/// `skipped`).
#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub ok: bool,
    pub action: String,
    pub dry_run: bool,
    pub refused: Vec<String>,
    pub warnings: Vec<String>,
    pub steps_run: usize,
    pub failure: Option<String>,
    pub world_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrote: Option<Vec<Reg>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub would_write: Option<Vec<Reg>>,
    pub reads: Vec<Reg>,
    pub undo: Vec<Reg>,
    pub undo_complete: bool,
    pub undo_gaps: Vec<String>,
    pub notes: Vec<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Report {
    fn with_writes(mut self, writes: Vec<Reg>) -> Self {
        if self.dry_run {
            self.would_write = Some(writes);
        } else {
            self.wrote = Some(writes);
        }
        self
    }

    pub fn refused(action: &str, reasons: Vec<String>, dry_run: bool) -> Self {
        Report {
            action: action.to_string(),
            dry_run,
            refused: reasons,
            undo_complete: true,
            ..Report::default()
        }
        .with_writes(Vec::new())
    }

    pub fn ran(plan: &Plan, out: &Outcome, warnings: Vec<String>, dry_run: bool) -> Self {
        let reached = plan
            .steps
            .iter()
            .take(out.steps_run)
            .filter_map(|s| match *s {
                Step::Write { vcp, value } => Some((vcp, value)),
                _ => None,
            });
        Report {
            ok: out.ok(),
            action: out.plan.clone(),
            dry_run,
            warnings,
            steps_run: out.steps_run,
            failure: out.first_failure.as_ref().map(|f| f.to_string()),
            world_changed: out.world_changed,
            reads: regs(out.reads.iter().copied()),
            undo: out
                .undo
                .as_ref()
                .map(|u| regs(u.writes()))
                .unwrap_or_default(),
            undo_complete: out.undo_is_complete(),
            undo_gaps: out
                .snapshot_gaps
                .iter()
                .map(|c| format!("0x{c:02X}"))
                .collect(),
            ..Report::default()
        }
        .with_writes(regs(reached))
    }

    pub fn code(&self) -> u8 {
        if !self.refused.is_empty() {
            exit::REFUSED
        } else if self.ok {
            exit::OK
        } else {
            exit::FAILED
        }
    }

    pub fn print(&self, ctx: &Ctx) -> u8 {
        if ctx.json {
            print_json(self);
        } else {
            self.print_text();
        }
        self.code()
    }

    fn print_text(&self) {
        for w in &self.warnings {
            eprintln!("warning: {w}");
        }
        if !self.refused.is_empty() {
            for r in &self.refused {
                eprintln!("refused: {r}");
            }
            eprintln!("nothing was written");
            return;
        }
        println!("{}", self.action);
        let writes = self.wrote.iter().map(|w| ("wrote", w));
        let lines = writes
            .chain(self.would_write.iter().map(|w| ("would write", w)))
            .chain([("read", &self.reads)]);
        for (verb, list) in lines {
            for r in list {
                println!("  {verb:<11} {} = 0x{:04X}", r.hex, r.value);
            }
        }
        match &self.failure {
            None if self.dry_run => println!("  dry run, nothing written"),
            None => println!("  ok"),
            Some(f) if self.world_changed => {
                eprintln!("failed: {f}");
                eprintln!("the panel was written to and may be half-applied");
                if !self.undo.is_empty() {
                    eprintln!(
                        "to put it back: {}",
                        one_line(self.undo.iter().map(|r| (r.code, r.value)))
                    );
                }
                if !self.undo_gaps.is_empty() {
                    eprintln!(
                        "couldn't capture, so can't put back: {}",
                        self.undo_gaps.join(" ")
                    );
                }
            }
            Some(f) => eprintln!("failed: {f}\nnothing was written"),
        }
        for n in &self.notes {
            println!("  note: {n}");
        }
    }
}

/// Writes as one line: `0xE9=0x0024, 0x60=0x1B1B`.
pub fn one_line(writes: impl IntoIterator<Item = (u8, u16)>) -> String {
    writes
        .into_iter()
        .map(|(c, v)| format!("0x{c:02X}=0x{v:04X}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ddc_core::fixture::idle_snapshot;
    use ddc_core::kvm;
    use ddc_transport::testing::{fast, u4323qe, WriteFails};

    fn json(r: &Report) -> Value {
        serde_json::from_str(&serde_json::to_string(r).unwrap()).unwrap()
    }

    #[test]
    fn a_report_parses_back_with_every_documented_field() {
        let mut d = fast(u4323qe());
        let mut runner = Runner::new(&mut d);
        let (r, _) = guarded(
            &mut runner,
            &idle_snapshot(),
            Some(&Intent::KvmToggle),
            &kvm::toggle_plan(),
            vec![],
            1,
        );
        let v = json(&r);
        for key in [
            "ok",
            "action",
            "dry_run",
            "refused",
            "warnings",
            "steps_run",
            "failure",
            "world_changed",
            "wrote",
            "reads",
            "undo",
            "undo_complete",
            "undo_gaps",
            "notes",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["ok"], true);
        assert_eq!(v["wrote"][0]["hex"], "0xE7");
        assert_eq!(v["wrote"][0]["value"], 0xFF00);
        assert!(v.get("would_write").is_none());
    }

    #[test]
    fn a_dry_run_reports_would_write_not_wrote() {
        let mut d = fast(u4323qe());
        let mut runner = Runner::new(&mut d).dry_run(true);
        let (r, _) = guarded(
            &mut runner,
            &idle_snapshot(),
            Some(&Intent::KvmToggle),
            &kvm::toggle_plan(),
            vec![],
            1,
        );
        let v = json(&r);
        assert!(v.get("wrote").is_none());
        assert_eq!(v["would_write"][0]["hex"], "0xE7");
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn a_refusal_has_the_same_shape_and_writes_nothing() {
        let mut d = fast(u4323qe().on_get(0xF2, 0x0080));
        let snap = Runner::new(&mut d).read_snapshot().unwrap();
        let mut runner = Runner::new(&mut d);
        let (r, out) = guarded(
            &mut runner,
            &snap,
            Some(&Intent::KvmToggle),
            &kvm::toggle_plan(),
            vec![],
            1,
        );
        assert!(out.is_none());
        assert_eq!(r.code(), exit::REFUSED);
        let v = json(&r);
        assert_eq!(v["ok"], false);
        assert!(v["refused"][0].as_str().unwrap().contains("busy"));
        assert_eq!(v["wrote"], serde_json::json!([]));
        assert!(d.transport.sets().is_empty());
    }

    #[test]
    fn a_failed_write_says_the_world_changed_and_offers_the_undo() {
        let mut d = fast(WriteFails(u4323qe()));
        let mut runner = Runner::new(&mut d);
        let snap = idle_snapshot();
        let plan = kvm::associate_plan(0x2740);
        let (r, _) = guarded(
            &mut runner,
            &snap,
            Some(&Intent::KvmAssociate { slot: 3 }),
            &plan,
            vec![],
            1,
        );
        assert_eq!(r.code(), exit::FAILED);
        assert!(r.world_changed);
        assert_eq!(r.wrote.as_deref().map(|w| w.len()), Some(1));
        assert_eq!(r.undo, regs([(0xE7, 0x2540)]));
        assert!(r.undo_complete);
    }

    #[test]
    fn a_verbs_own_refusal_stops_the_plan_before_the_guard() {
        let mut d = fast(u4323qe());
        let mut runner = Runner::new(&mut d);
        let extra = vec![Verdict::Refuse("no".into()), Verdict::Warn("hm".into())];
        let (r, _) = guarded(
            &mut runner,
            &idle_snapshot(),
            Some(&Intent::KvmToggle),
            &kvm::toggle_plan(),
            extra,
            1,
        );
        assert_eq!(r.refused, vec!["no"]);
        assert!(d.transport.writes.is_empty());
    }
}
