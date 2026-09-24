//! Executes a [`Plan`] against a live [`Ddc`] session.
//!
//! Two rules callers depend on:
//!
//! - **`world_changed` is set before a write, not after.** DDC sets are
//!   unacknowledged, so a write that errored may still have reached the panel.
//! - **The snapshot is taken just before the first write**, not before the
//!   first step. Reads before it see the same panel state anyway.

use std::time::{Duration, Instant};

use ddc_core::guard::{self, Intent, Snapshot};
use ddc_core::plan::{Failure, Outcome, Plan, Step};

use crate::{Ddc, Error, I2c};

/// How often [`Step::AwaitReady`] polls.
const POLL: Duration = Duration::from_millis(100);

/// Runs plans. Wrap a session, hand it a plan, read the [`Outcome`].
pub struct Runner<'a, T: I2c> {
    ddc: &'a mut Ddc<T>,
    dry_run: bool,
    max_dwell: Option<Duration>,
}

impl<'a, T: I2c> Runner<'a, T> {
    pub fn new(ddc: &'a mut Ddc<T>) -> Self {
        Runner {
            ddc,
            dry_run: false,
            max_dwell: None,
        }
    }

    /// Go through the motions without writing. Plain reads still happen;
    /// verifies and waits for the panel are skipped, since nothing changed.
    pub fn dry_run(mut self, yes: bool) -> Self {
        self.dry_run = yes;
        self
    }

    /// Whether this runner only plans writes. After a dry run a plan's writes
    /// are what *would* be written, not what was.
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Cap every dwell and wait. `Duration::ZERO` lets a test run a real plan
    /// in microseconds.
    pub fn max_dwell(mut self, d: Duration) -> Self {
        self.max_dwell = Some(d);
        self
    }

    fn capped(&self, d: Duration) -> Duration {
        self.max_dwell.map_or(d, |cap| d.min(cap))
    }

    /// Read a list of codes, skipping the ones that fail.
    ///
    /// Returns `(values, failed_codes)`. Several codes are optional, and one
    /// missing register should not stop an operation that does not need it.
    pub fn capture(&mut self, codes: &[u8]) -> (Vec<(u8, u16)>, Vec<u8>) {
        let mut values = Vec::new();
        let mut gaps = Vec::new();
        for code in codes {
            match self.ddc.get(*code) {
                Ok(r) => values.push((*code, r.current)),
                Err(_) => gaps.push(*code),
            }
        }
        (values, gaps)
    }

    /// Read the guard [`Snapshot`]: the capability string, then
    /// [`guard::READ_ORDER`].
    ///
    /// Only the capability string is fatal: treating an unread one as
    /// "advertises nothing" would refuse everything for the wrong reason.
    pub fn read_snapshot(&mut self) -> Result<Snapshot, Error> {
        let caps = self.ddc.capabilities()?;
        let (values, _gaps) = self.capture(&guard::READ_ORDER);
        Ok(Snapshot::from_reads(self.ddc.panel(), caps, &values))
    }

    /// Run a plan to completion, or to its first failure.
    pub fn run(&mut self, plan: &Plan) -> Outcome {
        let mut out = Outcome {
            plan: plan.name.clone(),
            steps_run: 0,
            first_failure: None,
            world_changed: false,
            snapshot: Vec::new(),
            snapshot_gaps: Vec::new(),
            reads: Vec::new(),
            undo: None,
        };
        let mut snapshotted = plan.snapshot.is_empty();

        for (i, step) in plan.steps.iter().enumerate() {
            if !snapshotted && step.is_write() {
                (out.snapshot, out.snapshot_gaps) = self.capture(&plan.snapshot);
                snapshotted = true;
            }
            out.steps_run = i + 1;
            if let Err(reason) = self.step(*step, &mut out) {
                out.first_failure = Some(Failure {
                    index: i,
                    step: *step,
                    reason,
                });
                break;
            }
        }

        if out.world_changed {
            out.undo = plan.undo_from(&out.snapshot);
        }
        out
    }

    fn step(&mut self, step: Step, out: &mut Outcome) -> Result<(), String> {
        match step {
            // Nothing was written, so there is nothing to verify or wait for.
            Step::Write { .. } | Step::Verify { .. } | Step::AwaitReady { .. } if self.dry_run => {
                Ok(())
            }
            Step::Write { vcp, value } => {
                // Before, not after: see the module docs.
                out.world_changed = true;
                self.ddc.set(vcp, value).map_err(|e| e.to_string())
            }
            Step::Read { vcp } => {
                let r = self.ddc.get(vcp).map_err(|e| e.to_string())?;
                out.reads.push((vcp, r.current));
                Ok(())
            }
            Step::Verify { vcp, expect } => {
                let r = self.ddc.get(vcp).map_err(|e| e.to_string())?;
                out.reads.push((vcp, r.current));
                if value_matches(r.current, expect) {
                    Ok(())
                } else {
                    Err(format!(
                        "read back 0x{:04X}, expected 0x{expect:04X}",
                        r.current
                    ))
                }
            }
            Step::AwaitReady { vcp, timeout } => self.await_ready(vcp, timeout, out),
            Step::Dwell(d) => {
                std::thread::sleep(self.capped(d));
                Ok(())
            }
        }
    }

    /// Poll until the panel answers, rather than sleeping a guess at how long
    /// it is unreachable.
    fn await_ready(&mut self, vcp: u8, timeout: Duration, out: &mut Outcome) -> Result<(), String> {
        let budget = self.capped(timeout);
        if budget.is_zero() {
            return Ok(());
        }
        let start = Instant::now();
        loop {
            let last = match self.ddc.get(vcp) {
                Ok(r) => {
                    out.reads.push((vcp, r.current));
                    return Ok(());
                }
                Err(e) => e,
            };
            if start.elapsed() >= budget {
                return Err(format!(
                    "0x{vcp:02X} did not become readable within {}ms: {last}",
                    budget.as_millis()
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Run a plan until it succeeds, up to `tries` attempts.
    ///
    /// This is how a wake is retried; Dell's software does the same, since a
    /// single power-on write can be dropped. The outcome is the last attempt's,
    /// with `world_changed` ORed across all of them.
    pub fn run_until_ok(&mut self, plan: &Plan, tries: u32) -> Outcome {
        let mut last = self.run(plan);
        let mut changed = last.world_changed;
        for _ in 1..tries.max(1) {
            if last.ok() {
                break;
            }
            last = self.run(plan);
            changed |= last.world_changed;
        }
        last.world_changed = changed;
        last
    }

    /// Guard first, then run. The entry point a command should use.
    ///
    /// On a refusal nothing is issued at all. Warnings come back alongside a
    /// successful outcome so the caller can print them.
    pub fn apply(
        &mut self,
        snap: &Snapshot,
        intent: &Intent,
        plan: &Plan,
    ) -> Result<(Outcome, Vec<String>), Vec<String>> {
        self.apply_with_tries(snap, intent, plan, 1)
    }

    /// [`apply`](Self::apply), but retry the plan up to `tries` times like
    /// [`run_until_ok`](Self::run_until_ok). The guard runs once.
    pub fn apply_with_tries(
        &mut self,
        snap: &Snapshot,
        intent: &Intent,
        plan: &Plan,
        tries: u32,
    ) -> Result<(Outcome, Vec<String>), Vec<String>> {
        let mut verdicts = guard::evaluate(snap, intent);
        verdicts.extend(guard::check_plan(snap, plan));
        let verdicts = guard::tidy(verdicts);
        let refusals = guard::refusals_of(&verdicts);
        if !refusals.is_empty() {
            return Err(refusals);
        }
        Ok((
            self.run_until_ok(plan, tries),
            guard::warnings_of(&verdicts),
        ))
    }
}

/// Does a readback satisfy a `Verify`?
///
/// Exact match, or the low byte matches a single-byte expectation: this panel
/// duplicates enumerated values across both bytes (0x60 reads `0x1B1B` for USB-C).
/// Looser than [`ddc_core::vcp::same_reading`] on purpose: it masks any byte-sized
/// write (PowerNap's 0xE0/0xE1 too), not just the profile's enumerated codes.
fn value_matches(current: u16, expect: u16) -> bool {
    current == expect || (expect <= 0xFF && current & 0xFF == expect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{fast, DeadI2c, Op, ReplayI2c};
    use ddc_core::fixture::U4323QE;
    use ddc_core::plan::Plan;
    use ddc_core::power::{self, PowerNapType};
    use ddc_core::vcp::input;
    use ddc_core::Capabilities;
    use ddc_core::{kvm, pxp};

    /// A fake panel in the state this one actually rests in.
    fn live_panel() -> ReplayI2c {
        ReplayI2c::panel()
            .on_get(0xF2, 0x0000)
            .on_get(0xF1, 0xC12B)
            .on_get(0xE9, 0x0024)
            .on_get_max(0x60, 0x1B1B, 0x000E)
            .on_get(0xEE, 0xBA98)
            .on_get(0xE7, 0x2540)
            .on_get(0xE8, 0x6DF1)
            .on_get(0xD6, 0x0001)
            .on_get(0xE0, 0x0000)
            .on_get(0xE1, 0x0000)
    }

    fn snap() -> ddc_core::guard::Snapshot {
        ddc_core::guard::Snapshot::from_reads(
            ddc_core::vcp::default_panel(),
            Capabilities::parse(U4323QE),
            &[
                (0xF2, 0x0000),
                (0xF1, 0xC12B),
                (0xE9, 0x0024),
                (0x60, 0x1B1B),
                (0xEE, 0xBA98),
                (0xE7, 0x2540),
                (0xE8, 0x6DF1),
            ],
        )
    }

    #[test]
    fn kvm_commit_reaches_the_wire_in_ddpms_order() {
        let mut d = fast(live_panel());
        let plan = kvm::commit_plan(input::DP2, 0x6DFB, 0x24, 0x2540);
        let out = Runner::new(&mut d).max_dwell(Duration::ZERO).run(&plan);

        assert!(out.ok(), "{out}");
        d.transport.assert_sets(&[
            (0x60, 0x0013),
            (0xE8, 0x6DFB),
            (0xE9, 0x0024),
            (0xE8, 0x6DFB),
            (0xE7, 0x2540),
            (0xE8, 0x6DFB),
        ]);
        // And every one of those frames went out twice. No exceptions.
        assert!(d.transport.every_frame_doubled());
    }

    #[test]
    fn the_snapshot_is_read_before_the_first_write() {
        let mut d = fast(live_panel());
        let plan = pxp::apply_plan(0x21, input::DP2, 0x6DFB, &[]);
        let out = Runner::new(&mut d).max_dwell(Duration::ZERO).run(&plan);

        // Every snapshot read precedes every write, in the op stream.
        let ops = d.transport.ops();
        let first_set = ops.iter().position(|o| matches!(o, Op::Set(..))).unwrap();
        for code in &plan.snapshot {
            let read_at = ops
                .iter()
                .position(|o| matches!(o, Op::Get(v) if v == code))
                .unwrap_or_else(|| panic!("0x{code:02X} was never snapshotted"));
            assert!(
                read_at < first_set,
                "0x{code:02X} read after the first write"
            );
        }
        assert_eq!(out.snapshot.len(), 3);
        assert!(out.snapshot_gaps.is_empty());
        assert!(out.undo_is_complete());
    }

    #[test]
    fn undo_restores_the_captured_state() {
        let mut d = fast(live_panel());
        let plan = pxp::apply_plan(0x21, input::DP2, 0x6DFB, &[]);
        let out = Runner::new(&mut d).max_dwell(Duration::ZERO).run(&plan);

        let undo = out
            .undo
            .expect("a write happened, so there must be an undo");
        assert_eq!(
            undo.writes(),
            vec![(0xE9, 0x0024), (0x60, 0x1B1B), (0xE8, 0x6DF1)]
        );

        // Running the undo puts the fake panel back where it started.
        d.transport.clear_writes();
        let back = Runner::new(&mut d).max_dwell(Duration::ZERO).run(&undo);
        assert!(back.ok());
        assert_eq!(d.transport.value_of(0xE9), Some(0x0024));
        assert_eq!(d.transport.value_of(0x60), Some(0x1B1B));
        assert_eq!(d.transport.value_of(0xE8), Some(0x6DF1));
    }

    #[test]
    fn a_failure_before_any_write_reports_nothing_changed() {
        // A plan that reads first; the read fails, so no write is ever reached.
        let mut d = fast(ReplayI2c::panel());
        let plan = Plan::new("look then leap")
            .snapshotting(&[0x60])
            .read(0x60)
            .write(0x60, 0x0013);
        let out = Runner::new(&mut d).run(&plan);

        assert!(!out.ok());
        assert!(!out.world_changed, "nothing was written");
        assert!(!out.partial());
        assert!(out.undo.is_none());
        assert_eq!(out.steps_run, 1);
        assert!(d.transport.sets().is_empty());
        assert!(format!("{out}").contains("nothing was written"));
    }

    #[test]
    fn a_failure_after_a_write_admits_the_world_changed() {
        // Writes succeed; the verify at the end does not.
        let mut d = fast(live_panel());
        let plan = Plan::new("half")
            .snapshotting(&[0xE9])
            .write(0xE9, 0x0021)
            .verify(0xE9, 0x0041); // never going to match
        let out = Runner::new(&mut d).run(&plan);

        assert!(!out.ok());
        assert!(out.world_changed);
        assert!(out.partial());
        assert!(format!("{out}").contains("half-applied"));
        // And the undo that would put it back is right there.
        assert_eq!(out.undo.unwrap().writes(), vec![(0xE9, 0x0024)]);
    }

    #[test]
    fn a_transport_error_on_a_write_still_counts_as_changed() {
        // DDC sets are unacknowledged: the frame may have landed anyway.
        let mut d = fast(DeadI2c);
        let out = Runner::new(&mut d).run(&Plan::new("doomed").write(0x60, 0x1B));
        assert!(!out.ok());
        assert!(
            out.world_changed,
            "an errored set may still have reached the panel"
        );
    }

    #[test]
    fn verify_accepts_the_duplicated_byte_this_panel_returns() {
        // 0x60 reads back 0x1B1B after a 0x001B write. An exact-match verify
        // would call a successful write a failure.
        assert!(value_matches(0x1B1B, 0x001B));
        assert!(value_matches(0x0024, 0x0024));
        assert!(!value_matches(0x1B1B, 0x0013));

        let mut d = fast(live_panel());
        let plan = Plan::new("switch").write(0x60, 0x001B).verify(0x60, 0x001B);
        assert!(Runner::new(&mut d).run(&plan).ok());
    }

    #[test]
    fn dry_run_reads_but_never_writes() {
        let mut d = fast(live_panel());
        let plan = kvm::commit_plan(input::DP2, 0x6DFB, 0x24, 0x2540);
        let out = Runner::new(&mut d)
            .dry_run(true)
            .max_dwell(Duration::ZERO)
            .run(&plan);

        assert!(out.ok());
        assert!(!out.world_changed);
        assert!(
            d.transport.sets().is_empty(),
            "a dry run wrote to the panel"
        );
        assert!(out.undo.is_none());
    }

    #[test]
    fn dry_run_skips_verifying_a_write_it_never_made() {
        let mut d = fast(live_panel());
        let plan = Plan::new("switch").write(0x60, 0x0013).verify(0x60, 0x0013);
        let out = Runner::new(&mut d).dry_run(true).run(&plan);
        assert!(out.ok(), "{out}");
        assert!(d.transport.writes.is_empty());
    }

    #[test]
    fn wake_retries_the_whole_block() {
        // 0xD6 stuck at 0x05 (off): every attempt's verify fails.
        let panel = live_panel().on_get(0xD6, 0x0005);
        let mut panel = panel;
        panel.echo_writes = false; // a dropped power-on write is the point
        let mut d = fast(panel);
        let plan = power::wake_plan(PowerNapType::TwoBooleans, None);
        let out = Runner::new(&mut d)
            .max_dwell(Duration::ZERO)
            .run_until_ok(&plan, 3);

        assert!(!out.ok());
        assert!(out.world_changed);
        // Three attempts, three power-on writes.
        assert_eq!(
            d.transport
                .sets()
                .iter()
                .filter(|(c, _)| *c == 0xD6)
                .count(),
            3
        );
        d.transport.assert_code_order(&[0xD6, 0xE0, 0xE1, 0xD6]);
    }

    #[test]
    fn wake_stops_as_soon_as_the_panel_is_awake() {
        let mut d = fast(live_panel()); // 0xD6 already reads 0x01
        let plan = power::wake_plan(PowerNapType::TwoBooleans, None);
        let out = Runner::new(&mut d)
            .max_dwell(Duration::ZERO)
            .run_until_ok(&plan, 3);
        assert!(out.ok());
        assert_eq!(
            d.transport
                .sets()
                .iter()
                .filter(|(c, _)| *c == 0xD6)
                .count(),
            1
        );
    }

    #[test]
    fn apply_refuses_without_issuing_a_single_frame() {
        let mut d = fast(live_panel());
        let mut s = snap();
        s.status = Some(0x0080); // OSD open

        let plan = pxp::apply_plan(0x21, input::DP2, 0x6DFB, &[]);
        let intent = Intent::SetLayout { layout: 0x21 };
        let err = Runner::new(&mut d)
            .max_dwell(Duration::ZERO)
            .apply(&s, &intent, &plan)
            .unwrap_err();

        assert!(err[0].contains("busy"));
        assert!(
            d.transport.writes.is_empty(),
            "a refusal must issue no traffic"
        );
    }

    #[test]
    fn apply_runs_and_forwards_warnings() {
        let mut d = fast(live_panel());
        let mut s = snap();
        s.status = None; // both the intent and the plan check warn about this
        let plan = kvm::toggle_plan();
        let (out, warnings) = Runner::new(&mut d)
            .max_dwell(Duration::ZERO)
            .apply(&s, &Intent::KvmToggle, &plan)
            .unwrap();

        assert!(out.ok());
        assert_eq!(d.transport.sets(), vec![(0xE7, 0xFF00)]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("0xF2"));
    }

    #[test]
    fn apply_refuses_a_plan_that_writes_an_unadvertised_code() {
        let mut d = fast(live_panel());
        let s = snap();
        let plan = Plan::new("audio source").write(0x63, 0x00F1);
        let err = Runner::new(&mut d)
            .apply(
                &s,
                &Intent::Raw {
                    vcp: 0x63,
                    value: 0x00F1,
                },
                &plan,
            )
            .unwrap_err();
        assert!(err.iter().any(|m| m.contains("0x63")));
        assert!(d.transport.writes.is_empty());
    }

    #[test]
    fn read_snapshot_uses_the_documented_order() {
        let mut d = fast(crate::testing::u4323qe());
        let _ = Runner::new(&mut d).read_snapshot().unwrap();
        assert_eq!(d.transport.gets(), ddc_core::guard::READ_ORDER.to_vec());
        // 0xF2 first: a busy panel is discovered before six more round-trips.
        assert_eq!(d.transport.gets()[0], 0xF2);
        // and the layout before the sub-sources it makes sense of.
        let pos = |c: u8| d.transport.gets().iter().position(|g| *g == c).unwrap();
        assert!(pos(0xE9) < pos(0xE8));
    }

    #[test]
    fn snapshot_gaps_are_named_rather_than_silently_dropped() {
        // 0xE7 is not seeded, so it reads back as a Null Message and fails.
        let panel = ReplayI2c::panel().on_get(0x60, 0x1B1B).on_get(0xE8, 0x6DF1);
        let mut d = fast(panel);
        let plan = Plan::new("three")
            .snapshotting(&[0x60, 0xE7, 0xE8])
            .write(0x60, 0x0013)
            .write(0xE7, 0x2540)
            .write(0xE8, 0x6DFB);
        let out = Runner::new(&mut d).run(&plan);

        assert_eq!(out.snapshot_gaps, vec![0xE7]);
        assert!(!out.undo_is_complete());
        // The undo covers what it can and does not invent the rest.
        assert_eq!(
            out.undo.unwrap().writes(),
            vec![(0x60, 0x1B1B), (0xE8, 0x6DF1)]
        );
    }

    #[test]
    fn dwells_are_honoured_and_cappable() {
        let plan = kvm::commit_plan(input::DP2, 0x6DFB, 0x24, 0x2540);
        assert_eq!(plan.total_dwell(), Duration::from_secs(5));

        let mut d = fast(live_panel());
        let start = std::time::Instant::now();
        Runner::new(&mut d).max_dwell(Duration::ZERO).run(&plan);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the cap was ignored"
        );
    }
}
