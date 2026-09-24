//! Multi-step operations as data.
//!
//! A [`Plan`] is a name, the codes to snapshot before the first write, and a
//! list of [`Step`]s. Building one issues no traffic, so a whole sequence can be
//! checked in a unit test; `ddc_transport::Runner` is the only thing that runs
//! one against a panel.
//!
//! After a failure, look at [`Outcome::world_changed`]: a plan that stopped
//! before its first write changed nothing, one that stopped halfway may have.

use std::fmt;
use std::time::Duration;

use crate::vcp::Vcp;

/// How long [`Step::AwaitReady`] waits for the panel after a layout or input
/// write. A timeout, not a sleep: a ready panel costs one read.
pub const LAYOUT_SETTLE: Duration = Duration::from_millis(6000);

/// Undo restores these first, in this order. The layout re-derives the
/// sub-sources, so it has to land before 0xE8 does.
const UNDO_ORDER: [u8; 4] = [
    Vcp::PIP_MODE,
    Vcp::INPUT_SOURCE,
    Vcp::PIP_SUB_SOURCE,
    Vcp::USB_KVM,
];

/// One instruction in a [`Plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Set a VCP feature. DDC sets are unacknowledged, so this is the step that
    /// makes [`Outcome::world_changed`] true.
    Write { vcp: u8, value: u16 },
    /// Read a VCP feature into [`Outcome::reads`]. Fails only on the transport.
    Read { vcp: u8 },
    /// Read a VCP feature and fail unless it equals `expect`.
    ///
    /// Compare against the canonical value where the panel normalises writes
    /// (0xE9 folds 0x01/0x02 to 0x32/0x24), or a good write fails its verify.
    Verify { vcp: u8, expect: u16 },
    /// Wait a fixed time.
    Dwell(Duration),
    /// Poll `vcp` until it answers or `timeout` passes. Used after layout and
    /// input writes in case the panel re-syncs; it normally answers at once.
    AwaitReady { vcp: u8, timeout: Duration },
}

impl Step {
    /// The VCP code this step touches, if any.
    pub fn vcp(&self) -> Option<u8> {
        match *self {
            Step::Write { vcp, .. }
            | Step::Read { vcp }
            | Step::Verify { vcp, .. }
            | Step::AwaitReady { vcp, .. } => Some(vcp),
            Step::Dwell(_) => None,
        }
    }
    /// Whether this step changes the panel.
    pub fn is_write(&self) -> bool {
        matches!(self, Step::Write { .. })
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Step::Write { vcp, value } => write!(f, "set 0x{vcp:02X} = 0x{value:04X}"),
            Step::Read { vcp } => write!(f, "get 0x{vcp:02X}"),
            Step::Verify { vcp, expect } => write!(f, "verify 0x{vcp:02X} == 0x{expect:04X}"),
            Step::Dwell(d) => write!(f, "dwell {}ms", d.as_millis()),
            Step::AwaitReady { vcp, timeout } => {
                write!(
                    f,
                    "await 0x{vcp:02X} readable (<= {}ms)",
                    timeout.as_millis()
                )
            }
        }
    }
}

/// A named, ordered sequence of [`Step`]s plus the codes to capture first.
///
/// `snapshot` lists the codes a runner reads before the first write. Those
/// values are what [`undo_from`](Plan::undo_from) restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    /// VCP codes to read before the first write, in read order.
    pub snapshot: Vec<u8>,
    pub steps: Vec<Step>,
    /// Set by [`Plan::snapshot_writes`]: every later write joins `snapshot`.
    snapshot_all_writes: bool,
}

impl Plan {
    pub fn new(name: impl Into<String>) -> Self {
        Plan {
            name: name.into(),
            snapshot: Vec::new(),
            steps: Vec::new(),
            snapshot_all_writes: false,
        }
    }

    /// Add codes to capture before the first write. Duplicates are dropped.
    pub fn snapshotting(mut self, codes: &[u8]) -> Self {
        for c in codes {
            if !self.snapshot.contains(c) {
                self.snapshot.push(*c);
            }
        }
        self
    }

    /// Also snapshot every code this plan writes, before or after this call.
    pub fn snapshot_writes(mut self) -> Self {
        self.snapshot_all_writes = true;
        let written = self.written_codes();
        self.snapshotting(&written)
    }

    pub fn write(self, vcp: u8, value: u16) -> Self {
        self.step(Step::Write { vcp, value })
    }
    pub fn read(self, vcp: u8) -> Self {
        self.step(Step::Read { vcp })
    }
    pub fn verify(self, vcp: u8, expect: u16) -> Self {
        self.step(Step::Verify { vcp, expect })
    }
    pub fn dwell(self, d: Duration) -> Self {
        self.step(Step::Dwell(d))
    }
    /// Wait for `vcp` to answer, up to `timeout`. See [`Step::AwaitReady`].
    pub fn await_ready(self, vcp: u8, timeout: Duration) -> Self {
        self.step(Step::AwaitReady { vcp, timeout })
    }
    pub fn step(mut self, s: Step) -> Self {
        if let Step::Write { vcp, .. } = s {
            if self.snapshot_all_writes && !self.snapshot.contains(&vcp) {
                self.snapshot.push(vcp);
            }
        }
        self.steps.push(s);
        self
    }

    /// Codes this plan writes, in first-written order, without duplicates.
    pub fn written_codes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for s in &self.steps {
            if let Step::Write { vcp, .. } = *s {
                if !out.contains(&vcp) {
                    out.push(vcp);
                }
            }
        }
        out
    }

    /// Every (vcp, value) write in wire order, duplicates kept.
    pub fn writes(&self) -> Vec<(u8, u16)> {
        self.steps
            .iter()
            .filter_map(|s| match *s {
                Step::Write { vcp, value } => Some((vcp, value)),
                _ => None,
            })
            .collect()
    }

    /// Does this plan change anything at all?
    pub fn is_read_only(&self) -> bool {
        !self.steps.iter().any(Step::is_write)
    }

    /// Total time the dwells alone will cost.
    pub fn total_dwell(&self) -> Duration {
        self.steps
            .iter()
            .filter_map(|s| match *s {
                Step::Dwell(d) => Some(d),
                _ => None,
            })
            .sum()
    }

    /// Build the plan that puts back what this one changed.
    ///
    /// `captured` is what a runner read from [`Plan::snapshot`]. Only codes this
    /// plan wrote are restored; a snapshot can cover more (0xE9 is read to make
    /// sense of 0xE8) and rewriting an untouched code is its own hazard.
    ///
    /// Restores run in reverse write order, except that 0xE9, 0x60, 0xE8 and
    /// 0xE7 always go in that order among themselves, since a layout write
    /// clobbers the sub-sources. Restores
    /// are separated by the longest dwell the forward plan used, and a code that
    /// was followed by an [`Step::AwaitReady`] going forward gets one here too.
    pub fn undo_from(&self, captured: &[(u8, u16)]) -> Option<Plan> {
        let mut order = self.written_codes();
        order.reverse();
        let ranked: Vec<u8> = UNDO_ORDER
            .iter()
            .copied()
            .filter(|c| order.contains(c))
            .collect();
        for (slot, code) in order
            .iter_mut()
            .filter(|c| UNDO_ORDER.contains(c))
            .zip(ranked)
        {
            *slot = code;
        }

        let dwell = self
            .steps
            .iter()
            .filter_map(|s| match *s {
                Step::Dwell(d) => Some(d),
                _ => None,
            })
            .max()
            .unwrap_or(Duration::ZERO);
        let await_for = |vcp: u8| {
            self.steps.iter().find_map(|s| match *s {
                Step::AwaitReady { vcp: v, timeout } if v == vcp => Some(timeout),
                _ => None,
            })
        };

        let mut undo = Plan::new(format!("undo {}", self.name));
        for vcp in order {
            let Some(&(_, value)) = captured.iter().find(|(c, _)| *c == vcp) else {
                continue;
            };
            if !undo.steps.is_empty() && !dwell.is_zero() {
                undo = undo.dwell(dwell);
            }
            undo = undo.write(vcp, value);
            if let Some(timeout) = await_for(vcp) {
                undo = undo.await_ready(vcp, timeout);
            }
        }
        (!undo.steps.is_empty()).then_some(undo)
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "plan {}", self.name)?;
        for (i, s) in self.steps.iter().enumerate() {
            writeln!(f, "  {i}. {s}")?;
        }
        Ok(())
    }
}

/// Why a plan stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Index into [`Plan::steps`].
    pub index: usize,
    pub step: Step,
    /// Transport or verify detail, already rendered so `ddc-core` doesn't
    /// depend on a transport's error type.
    pub reason: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {} ({}): {}", self.index, self.step, self.reason)
    }
}

/// What running a plan did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub plan: String,
    /// Steps actually executed, including the one that failed.
    pub steps_run: usize,
    pub first_failure: Option<Failure>,
    /// True once any write has been attempted. A DDC set is unacknowledged, so
    /// a write that errored may still have reached the panel.
    pub world_changed: bool,
    /// Values captured from [`Plan::snapshot`] before the first write.
    pub snapshot: Vec<(u8, u16)>,
    /// Snapshot codes that couldn't be read, so [`Outcome::undo`] can't restore
    /// them.
    pub snapshot_gaps: Vec<u8>,
    /// Every value read by a `Read`, `Verify` or `AwaitReady` step, in order.
    pub reads: Vec<(u8, u16)>,
    /// A plan that restores the snapshot, when something was written.
    pub undo: Option<Plan>,
}

impl Outcome {
    /// Ran to the end with no failure.
    pub fn ok(&self) -> bool {
        self.first_failure.is_none()
    }
    /// Failed after writing, so the panel may be half-applied.
    pub fn partial(&self) -> bool {
        self.first_failure.is_some() && self.world_changed
    }
    /// Whether [`Outcome::undo`] can put back everything this plan wrote.
    pub fn undo_is_complete(&self) -> bool {
        self.snapshot_gaps.is_empty()
    }
    /// The last value read for `vcp`, if any step read it.
    pub fn value_of(&self, vcp: u8) -> Option<u16> {
        self.reads
            .iter()
            .rev()
            .find(|(c, _)| *c == vcp)
            .map(|(_, v)| *v)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.first_failure {
            None => write!(f, "{}: {} step(s), ok", self.plan, self.steps_run),
            Some(fail) => write!(
                f,
                "{}: failed at {} after {} step(s); {}",
                self.plan,
                fail,
                self.steps_run,
                if self.world_changed {
                    "the panel was written to and may be half-applied"
                } else {
                    "nothing was written"
                }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a PxP apply: layout, wait, input, wait, sub-sources.
    fn apply_like() -> Plan {
        Plan::new("apply")
            .snapshotting(&[0xE9, 0x60, 0xE8])
            .write(0xE9, 0x0041)
            .await_ready(0xE9, LAYOUT_SETTLE)
            .write(0x60, 0x0013)
            .await_ready(0x60, LAYOUT_SETTLE)
            .write(0xE8, 0x6DF3)
    }

    #[test]
    fn undo_restores_the_layout_before_the_sub_sources() {
        let undo = apply_like()
            .undo_from(&[(0xE9, 0x0024), (0x60, 0x1B1B), (0xE8, 0x6DF1)])
            .unwrap();
        assert_eq!(
            undo.writes(),
            vec![(0xE9, 0x0024), (0x60, 0x1B1B), (0xE8, 0x6DF1)]
        );
        assert_eq!(undo.name, "undo apply");
    }

    #[test]
    fn undo_waits_for_the_panel_where_the_forward_plan_did() {
        let undo = apply_like()
            .undo_from(&[(0xE9, 0x0024), (0x60, 0x1B1B), (0xE8, 0x6DF1)])
            .unwrap();
        assert_eq!(
            undo.steps[1],
            Step::AwaitReady {
                vcp: 0xE9,
                timeout: LAYOUT_SETTLE
            }
        );
        assert_eq!(
            undo.steps[3],
            Step::AwaitReady {
                vcp: 0x60,
                timeout: LAYOUT_SETTLE
            }
        );
        assert!(undo.steps[4].is_write());
    }

    #[test]
    fn undo_of_a_kvm_style_commit_writes_0xe9_before_0xe8() {
        // 0xE8 is written before and after 0xE9 going forward. Restoring 0xE8
        // first would let the layout restore clobber it.
        let d = Duration::from_millis(1000);
        let p = Plan::new("commit")
            .snapshotting(&[0x60, 0xE8, 0xE9, 0xE7])
            .write(0x60, 0x13)
            .dwell(d)
            .write(0xE8, 0x6DF1)
            .dwell(d)
            .write(0xE9, 0x24)
            .dwell(d)
            .write(0xE8, 0x6DF1)
            .dwell(d)
            .write(0xE7, 0x2540)
            .dwell(d)
            .write(0xE8, 0x6DF1);
        let undo = p
            .undo_from(&[
                (0x60, 0x0F0F),
                (0xE8, 0x6DFB),
                (0xE9, 0x0000),
                (0xE7, 0x2500),
            ])
            .unwrap();
        assert_eq!(
            undo.writes(),
            vec![
                (0xE9, 0x0000),
                (0x60, 0x0F0F),
                (0xE8, 0x6DFB),
                (0xE7, 0x2500)
            ]
        );
        // Restores are spaced by the forward dwell.
        for pair in undo.steps.windows(2) {
            assert!(!(pair[0].is_write() && pair[1].is_write()));
        }
        assert_eq!(undo.total_dwell(), d * 3);
    }

    #[test]
    fn undo_puts_other_codes_back_in_reverse() {
        let p = Plan::new("levels")
            .snapshot_writes()
            .write(0x10, 50)
            .write(0x12, 60);
        let undo = p.undo_from(&[(0x10, 40), (0x12, 70)]).unwrap();
        assert_eq!(undo.writes(), vec![(0x12, 70), (0x10, 40)]);

        // Other codes keep their place; only the PxP/KVM codes are reordered.
        let p = Plan::new("mixed")
            .snapshot_writes()
            .write(0xE8, 1)
            .write(0x10, 2)
            .write(0xE9, 3);
        let undo = p.undo_from(&[(0xE8, 10), (0x10, 20), (0xE9, 30)]).unwrap();
        assert_eq!(undo.writes(), vec![(0xE9, 30), (0x10, 20), (0xE8, 10)]);
    }

    #[test]
    fn undo_only_covers_codes_that_were_written() {
        // 0xE9 is snapshotted to interpret 0xE8 but never written here.
        let p = Plan::new("sub only")
            .snapshotting(&[0xE9, 0xE8])
            .write(0xE8, 0x6DF1);
        let undo = p.undo_from(&[(0xE9, 0x0024), (0xE8, 0x6DFB)]).unwrap();
        assert_eq!(undo.writes(), vec![(0xE8, 0x6DFB)]);
    }

    #[test]
    fn undo_of_a_read_only_plan_is_none() {
        let p = Plan::new("look")
            .snapshotting(&[0x60])
            .read(0x60)
            .read(0xE9);
        assert!(p.is_read_only());
        assert!(p.undo_from(&[(0x60, 0x1B1B)]).is_none());
    }

    #[test]
    fn undo_skips_codes_with_no_captured_value() {
        // A failed snapshot read leaves the code out; undo must not invent one.
        let p = Plan::new("two")
            .snapshot_writes()
            .write(0x60, 0x1B)
            .write(0xE9, 0x24);
        let undo = p.undo_from(&[(0x60, 0x1B1B)]).unwrap();
        assert_eq!(undo.writes(), vec![(0x60, 0x1B1B)]);
    }

    #[test]
    fn snapshot_writes_covers_writes_either_side_of_the_call() {
        let p = Plan::new("x")
            .snapshotting(&[0xE9])
            .write(0x60, 1)
            .snapshot_writes()
            .write(0xE8, 2);
        assert_eq!(p.snapshot, vec![0xE9, 0x60, 0xE8]);
        // Called first, it still picks up everything written after.
        let q = Plan::new("y")
            .snapshot_writes()
            .write(0x10, 1)
            .write(0x12, 2)
            .write(0x10, 3);
        assert_eq!(q.snapshot, vec![0x10, 0x12]);
        // Without it, writes aren't snapshotted.
        assert!(Plan::new("z").write(0x10, 1).snapshot.is_empty());
    }

    #[test]
    fn snapshot_deduplicates() {
        let p = Plan::new("x").snapshotting(&[0x60, 0xE9, 0x60]);
        assert_eq!(p.snapshot, vec![0x60, 0xE9]);
    }

    #[test]
    fn outcome_reports_partial_application() {
        let good = Outcome {
            plan: "x".into(),
            steps_run: 3,
            first_failure: None,
            world_changed: true,
            snapshot: vec![],
            snapshot_gaps: vec![],
            reads: vec![],
            undo: None,
        };
        assert!(good.ok());
        assert!(!good.partial());

        let half = Outcome {
            first_failure: Some(Failure {
                index: 2,
                step: Step::Write {
                    vcp: 0xE7,
                    value: 0x2540,
                },
                reason: "no reply".into(),
            }),
            ..good.clone()
        };
        assert!(!half.ok());
        assert!(half.partial());
        assert!(format!("{half}").contains("may be half-applied"));

        let untouched = Outcome {
            world_changed: false,
            ..half
        };
        assert!(!untouched.partial());
        assert!(format!("{untouched}").contains("nothing was written"));
    }

    #[test]
    fn steps_render_as_frames_a_human_can_check() {
        assert_eq!(
            Step::Write {
                vcp: 0xE9,
                value: 0x24
            }
            .to_string(),
            "set 0xE9 = 0x0024"
        );
        assert_eq!(Step::Read { vcp: 0x60 }.to_string(), "get 0x60");
        assert_eq!(
            Step::Dwell(Duration::from_millis(1000)).to_string(),
            "dwell 1000ms"
        );
    }
}
