//! Power mode (0xD6), PowerNap (0xE0/0xE1) and the wake block.
//!
//! A bare `0xD6 = 1` isn't a wake: a panel that went down through PowerNap
//! reads "on" and then goes straight back to sleep while its flags are still
//! set. [`wake_plan`] clears them too. Retry it with `Runner::run_until_ok`,
//! as Dell's software does.
//!
//! PowerNap has two encodings, picked from the capability string the way Dell's
//! software picks them ([`powernap_type`]). The U4323QE is the two-boolean kind;
//! the bitfield kind is inferred and untested.
//!
//! The monitor has no idea what a screensaver is. The host decides when to nap;
//! nothing here schedules anything or issues traffic.

use std::time::Duration;

use crate::caps::Capabilities;
use crate::guard::{Intent, Verdict};
use crate::plan::{Outcome, Plan};
use crate::vcp::Vcp;

/// VCP 0xE0: PowerNap dim, or the whole bitfield on some panels.
pub const POWERNAP_DIM: u8 = Vcp::POWERNAP_DIM;
/// VCP 0xE1: PowerNap sleep, on two-boolean panels.
pub const POWERNAP_SLEEP: u8 = Vcp::POWERNAP_SLEEP;

/// Bitfield PowerNap, bit 0: dim. Inferred from Dell's software.
pub const NAP_BIT_DIM: u16 = 1 << 0;
/// Bitfield PowerNap, bit 1: sleep. Inferred from Dell's software.
pub const NAP_BIT_SLEEP: u16 = 1 << 1;
/// Clears both nap bits and keeps the rest. Inferred from Dell's software.
pub const NAP_CLEAR_MASK: u16 = 0x00FC;

/// Pause between the two PowerNap writes, as Dell's software does.
pub const NAP_STEP: Duration = Duration::from_millis(100);
/// Quiet time around 0xD6/0xE0/0xE1 writes.
pub const POWER_SETTLE: Duration = Duration::from_millis(150);
/// How many times to run the wake block before giving up.
pub const WAKE_TRIES: u32 = 5;

// ---------------------------------------------------------------------------
// VCP 0xD6
// ---------------------------------------------------------------------------

/// A 0xD6 value. The U4323QE advertises `D6(01 04 05)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    On,
    /// MCCS "Off (reduced power)".
    Standby,
    /// MCCS "Off (power off)".
    Off,
    /// Anything else (MCCS 0x02 standby, 0x03 suspend).
    Other(u8),
}

/// Decode a 0xD6 reply. The value is in the low byte.
pub fn power_state(raw: u16) -> PowerState {
    match (raw & 0xFF) as u8 {
        0x01 => PowerState::On,
        0x04 => PowerState::Standby,
        0x05 => PowerState::Off,
        v => PowerState::Other(v),
    }
}

impl PowerState {
    pub fn code(self) -> u8 {
        match self {
            PowerState::On => 0x01,
            PowerState::Standby => 0x04,
            PowerState::Off => 0x05,
            PowerState::Other(v) => v,
        }
    }

    /// The name the panel profile uses.
    pub fn name(self) -> &'static str {
        match self {
            PowerState::On => "on",
            PowerState::Standby => "standby",
            PowerState::Off => "off",
            PowerState::Other(_) => "unknown",
        }
    }

    pub fn is_on(self) -> bool {
        matches!(self, PowerState::On)
    }

    pub fn resolve(text: &str) -> Option<PowerState> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" | "wake" => Some(PowerState::On),
            "standby" | "sleep" => Some(PowerState::Standby),
            "off" => Some(PowerState::Off),
            _ => None,
        }
    }
}

/// Write a power state. Only `on` is verified: a panel told to turn off owes
/// us no reply. Nothing is snapshotted, since undoing a power change isn't
/// something anyone wants.
pub fn power_plan(state: PowerState) -> Plan {
    let p = Plan::new(format!("power {}", state.name()))
        .write(Vcp::POWER_MODE, state.code() as u16)
        .dwell(POWER_SETTLE);
    if state.is_on() {
        p.verify(Vcp::POWER_MODE, 0x0001)
    } else {
        p
    }
}

// ---------------------------------------------------------------------------
// PowerNap
// ---------------------------------------------------------------------------

/// How a panel encodes PowerNap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerNapType {
    /// No `E0` advertised.
    Unsupported,
    /// Bare `E0` and `E1`: two independent booleans. The U4323QE.
    TwoBooleans,
    /// `E0` with a value list: one bitfield, bit 0 dim, bit 1 sleep.
    Bitfield,
}

/// Classify a panel from its capability string, not a read: a bitfield 0xE0
/// reads 0 or 1 just like a boolean one.
pub fn powernap_type(caps: &Capabilities) -> PowerNapType {
    match caps.legal_values(POWERNAP_DIM) {
        None => PowerNapType::Unsupported,
        Some(values) if !values.is_empty() => PowerNapType::Bitfield,
        Some(_) => PowerNapType::TwoBooleans,
    }
}

impl PowerNapType {
    pub fn label(self) -> &'static str {
        match self {
            PowerNapType::Unsupported => "unsupported",
            PowerNapType::TwoBooleans => "0xE0 dim / 0xE1 sleep",
            PowerNapType::Bitfield => "0xE0 bit 0 dim / bit 1 sleep",
        }
    }

    /// Whether this encoding has been tried on real hardware. Only the
    /// two-boolean one has.
    pub fn is_observed_here(self) -> bool {
        matches!(self, PowerNapType::TwoBooleans)
    }
}

/// What PowerNap does when the host's screensaver starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nap {
    Off,
    Dim,
    Sleep,
}

impl Nap {
    pub fn name(self) -> &'static str {
        match self {
            Nap::Off => "off",
            Nap::Dim => "dim",
            Nap::Sleep => "sleep",
        }
    }

    pub fn resolve(text: &str) -> Option<Nap> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "disable" => Some(Nap::Off),
            "dim" => Some(Nap::Dim),
            "sleep" => Some(Nap::Sleep),
            _ => None,
        }
    }

    fn bits(self) -> u16 {
        match self {
            Nap::Off => 0,
            Nap::Dim => NAP_BIT_DIM,
            Nap::Sleep => NAP_BIT_SLEEP,
        }
    }
}

/// The current PowerNap setting from raw 0xE0/0xE1 reads. `None` when the
/// panel has no PowerNap or a needed register couldn't be read.
pub fn nap_state(kind: PowerNapType, e0: Option<u16>, e1: Option<u16>) -> Option<Nap> {
    match kind {
        PowerNapType::Unsupported => None,
        PowerNapType::Bitfield => {
            let v = e0?;
            Some(if v & NAP_BIT_SLEEP != 0 {
                Nap::Sleep
            } else if v & NAP_BIT_DIM != 0 {
                Nap::Dim
            } else {
                Nap::Off
            })
        }
        PowerNapType::TwoBooleans => {
            let (dim, sleep) = (e0? & 0xFF != 0, e1? & 0xFF != 0);
            Some(match (dim, sleep) {
                (_, true) => Nap::Sleep,
                (true, false) => Nap::Dim,
                (false, false) => Nap::Off,
            })
        }
    }
}

/// The plan that sets PowerNap to `want`.
///
/// A bitfield panel needs `e0_current` because the write is read-modify-write;
/// without it there's no plan. The flag being turned on goes first, so both are
/// never set at once.
pub fn nap_plan(kind: PowerNapType, want: Nap, e0_current: Option<u16>) -> Option<Plan> {
    let name = format!("powernap {}", want.name());
    match kind {
        PowerNapType::Unsupported => None,
        PowerNapType::Bitfield => {
            let value = (e0_current? & NAP_CLEAR_MASK) | want.bits();
            Some(
                Plan::new(name)
                    .snapshotting(&[POWERNAP_DIM])
                    .write(POWERNAP_DIM, value)
                    .dwell(NAP_STEP)
                    .verify(POWERNAP_DIM, value),
            )
        }
        PowerNapType::TwoBooleans => {
            let ((c1, v1), (c2, v2)) = match want {
                Nap::Dim => ((POWERNAP_DIM, 1), (POWERNAP_SLEEP, 0)),
                Nap::Sleep => ((POWERNAP_SLEEP, 1), (POWERNAP_DIM, 0)),
                Nap::Off => ((POWERNAP_DIM, 0), (POWERNAP_SLEEP, 0)),
            };
            Some(
                Plan::new(name)
                    .snapshotting(&[POWERNAP_DIM, POWERNAP_SLEEP])
                    .write(c1, v1)
                    .dwell(NAP_STEP)
                    .write(c2, v2)
                    .dwell(NAP_STEP)
                    .verify(c1, v1)
                    .verify(c2, v2),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Wake
// ---------------------------------------------------------------------------

/// One attempt at the wake block: power on, clear PowerNap, verify 0xD6.
///
/// Two-boolean panels clear 0xE0 and 0xE1. Bitfield panels clear both bits of
/// 0xE0 in one masked write and leave 0xE1 alone (inferred); without
/// `e0_current` they skip the clear and [`wake_notes`] says so. 0xD6 isn't
/// snapshotted, so an undo never puts the panel back to sleep.
pub fn wake_plan(kind: PowerNapType, e0_current: Option<u16>) -> Plan {
    let mut p = Plan::new("wake")
        .write(Vcp::POWER_MODE, 0x0001)
        .dwell(POWER_SETTLE);
    match (kind, e0_current) {
        (PowerNapType::TwoBooleans, _) => {
            p = p
                .snapshotting(&[POWERNAP_DIM, POWERNAP_SLEEP])
                .write(POWERNAP_DIM, 0)
                .dwell(POWER_SETTLE)
                .write(POWERNAP_SLEEP, 0)
                .dwell(POWER_SETTLE);
        }
        (PowerNapType::Bitfield, Some(v)) => {
            p = p
                .snapshotting(&[POWERNAP_DIM])
                .write(POWERNAP_DIM, v & NAP_CLEAR_MASK)
                .dwell(POWER_SETTLE);
        }
        _ => {}
    }
    p.verify(Vcp::POWER_MODE, 0x0001)
}

/// The guard intent for a wake. [`Intent::Wake`] wants 0xE1 advertised, which a
/// bitfield panel may not have.
pub fn wake_intent(kind: PowerNapType) -> Intent {
    match kind {
        PowerNapType::TwoBooleans => Intent::Wake,
        _ => Intent::Raw {
            vcp: Vcp::POWER_MODE,
            value: 0x0001,
        },
    }
}

/// Caveats to print alongside a wake.
pub fn wake_notes(kind: PowerNapType, e0_current: Option<u16>) -> Vec<Verdict> {
    let mut out = Vec::new();
    match kind {
        PowerNapType::Unsupported => out.push(Verdict::Warn(String::from(
            "this panel doesn't advertise 0xE0, so PowerNap can't be cleared; if it went to \
             sleep some other way the power-on may not hold",
        ))),
        PowerNapType::Bitfield => {
            out.push(Verdict::Warn(String::from(
                "this panel's PowerNap is the bitfield kind, cleared with one masked write. \
                 inferred: untested on real hardware.",
            )));
            if e0_current.is_none() {
                out.push(Verdict::Warn(String::from(
                    "0xE0 couldn't be read, so PowerNap wasn't cleared; if the panel sleeps \
                     again straight away, that's why",
                )));
            }
        }
        PowerNapType::TwoBooleans => {}
    }
    out
}

/// Did the wake take? 0xD6 reports the state, so the last read settles it.
pub fn woke(out: &Outcome) -> Option<bool> {
    out.value_of(Vcp::POWER_MODE)
        .map(|v| power_state(v).is_on())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::U4323QE;
    use crate::plan::Step;

    /// DDPM's Type-03 shape: `E0` with a value list.
    const TYPE_03: &str =
        "(prot(monitor)type(lcd)model(FAKE)vcp(10 12 D6(01 04 05) E0(00 01 02 03))mccs_ver(2.1))";
    /// No PowerNap at all.
    const NO_NAP: &str = "(prot(monitor)type(lcd)model(FAKE)vcp(10 12 D6(01 04 05))mccs_ver(2.1))";

    fn caps(s: &str) -> Capabilities {
        Capabilities::parse(s)
    }

    #[test]
    fn the_type_comes_from_the_caps_string() {
        assert_eq!(powernap_type(&caps(U4323QE)), PowerNapType::TwoBooleans);
        assert_eq!(powernap_type(&caps(TYPE_03)), PowerNapType::Bitfield);
        assert_eq!(powernap_type(&caps(NO_NAP)), PowerNapType::Unsupported);
        assert!(PowerNapType::TwoBooleans.is_observed_here());
        assert!(!PowerNapType::Bitfield.is_observed_here());
    }

    #[test]
    fn a_two_boolean_wake_clears_both_flags_and_verifies() {
        let p = wake_plan(PowerNapType::TwoBooleans, Some(0x00));
        assert_eq!(
            p.writes(),
            vec![(0xD6, 0x0001), (0xE0, 0x0000), (0xE1, 0x0000)]
        );
        assert_eq!(
            *p.steps.last().unwrap(),
            Step::Verify {
                vcp: 0xD6,
                expect: 0x0001
            }
        );
        // Undoing a wake must not put the panel back to sleep.
        assert_eq!(p.snapshot, vec![0xE0, 0xE1]);
        assert_eq!(wake_intent(PowerNapType::TwoBooleans), Intent::Wake);
        assert!(wake_notes(PowerNapType::TwoBooleans, Some(0)).is_empty());
    }

    #[test]
    fn a_bitfield_wake_clears_with_the_mask_and_never_touches_0xe1() {
        let p = wake_plan(PowerNapType::Bitfield, Some(0x00A3));
        // 0xA3: both nap bits set, 0xA0 belongs to something else.
        assert_eq!(p.writes(), vec![(0xD6, 0x0001), (0xE0, 0x00A0)]);
        assert_eq!(
            wake_intent(PowerNapType::Bitfield),
            Intent::Raw {
                vcp: 0xD6,
                value: 1
            }
        );
        assert!(wake_notes(PowerNapType::Bitfield, Some(0))
            .iter()
            .any(|v| v.message().unwrap().contains("inferred")));
    }

    #[test]
    fn an_unreadable_bitfield_is_left_alone_and_said_so() {
        let p = wake_plan(PowerNapType::Bitfield, None);
        assert_eq!(p.writes(), vec![(0xD6, 0x0001)]);
        let notes = wake_notes(PowerNapType::Bitfield, None);
        assert_eq!(notes.len(), 2);
        assert!(notes[1].message().unwrap().contains("couldn't be read"));
    }

    #[test]
    fn wake_without_powernap_is_just_power_on_plus_verify() {
        let p = wake_plan(PowerNapType::Unsupported, None);
        assert_eq!(p.writes(), vec![(0xD6, 0x0001)]);
        assert!(matches!(
            p.steps.last(),
            Some(Step::Verify { vcp: 0xD6, .. })
        ));
        assert!(wake_notes(PowerNapType::Unsupported, None)[0]
            .message()
            .unwrap()
            .contains("doesn't advertise 0xE0"));
    }

    #[test]
    fn nap_plans_set_the_wanted_flag_before_clearing_the_other() {
        let plan = |want| {
            nap_plan(PowerNapType::TwoBooleans, want, None)
                .unwrap()
                .writes()
        };
        assert_eq!(plan(Nap::Dim), vec![(0xE0, 1), (0xE1, 0)]);
        assert_eq!(plan(Nap::Sleep), vec![(0xE1, 1), (0xE0, 0)]);
        assert_eq!(plan(Nap::Off), vec![(0xE0, 0), (0xE1, 0)]);
    }

    #[test]
    fn a_bitfield_nap_write_preserves_the_bits_it_does_not_own() {
        let p = nap_plan(PowerNapType::Bitfield, Nap::Sleep, Some(0x00A1)).unwrap();
        // 0xA1 & 0xFC = 0xA0, then bit 1 for sleep.
        assert_eq!(p.writes(), vec![(0xE0, 0x00A2)]);
        assert!(nap_plan(PowerNapType::Bitfield, Nap::Sleep, None).is_none());
        assert!(nap_plan(PowerNapType::Unsupported, Nap::Dim, Some(0)).is_none());
    }

    #[test]
    fn nap_state_decodes_each_encoding_and_refuses_to_guess() {
        let two = PowerNapType::TwoBooleans;
        assert_eq!(nap_state(two, Some(0), Some(0)), Some(Nap::Off));
        assert_eq!(nap_state(two, Some(1), Some(0)), Some(Nap::Dim));
        assert_eq!(nap_state(two, Some(0), Some(1)), Some(Nap::Sleep));
        assert_eq!(nap_state(two, Some(0), None), None);
        assert_eq!(nap_state(PowerNapType::Bitfield, None, Some(1)), None);
        assert_eq!(
            nap_state(PowerNapType::Bitfield, Some(0x02), None),
            Some(Nap::Sleep)
        );
        assert_eq!(
            nap_state(PowerNapType::Bitfield, Some(0xA1), None),
            Some(Nap::Dim)
        );
        assert_eq!(Nap::resolve("DIM"), Some(Nap::Dim));
        assert_eq!(Nap::resolve("nope"), None);
    }

    #[test]
    fn a_panel_without_powernap_has_no_nap_state() {
        assert_eq!(nap_state(PowerNapType::Unsupported, Some(0), Some(0)), None);
    }

    #[test]
    fn power_states_match_the_profile_and_only_on_is_verified() {
        for s in [PowerState::On, PowerState::Standby, PowerState::Off] {
            assert_eq!(
                crate::vcp::default_panel().value_name(Vcp::POWER_MODE, s.code()),
                Some(s.name())
            );
        }
        assert_eq!(power_state(0x0001), PowerState::On);
        assert_eq!(power_state(0x0005), PowerState::Off);
        assert_eq!(power_state(0x0002), PowerState::Other(2));
        assert_eq!(PowerState::resolve("standby"), Some(PowerState::Standby));

        let on = power_plan(PowerState::On);
        assert!(matches!(
            on.steps.last(),
            Some(Step::Verify { vcp: 0xD6, .. })
        ));
        let off = power_plan(PowerState::Off);
        assert_eq!(off.writes(), vec![(0xD6, 0x0005)]);
        assert!(!off.steps.iter().any(|s| matches!(s, Step::Verify { .. })));
        assert!(off.snapshot.is_empty());
    }

    #[test]
    fn woke_reads_the_state_not_the_write() {
        let with = |reads: Vec<(u8, u16)>| Outcome {
            plan: "wake".into(),
            steps_run: 0,
            first_failure: None,
            world_changed: true,
            snapshot: vec![],
            snapshot_gaps: vec![],
            reads,
            undo: None,
        };
        assert_eq!(woke(&with(vec![(0xD6, 0x0001)])), Some(true));
        assert_eq!(woke(&with(vec![(0xD6, 0x0005)])), Some(false));
        // The last read wins.
        assert_eq!(
            woke(&with(vec![(0xD6, 0x0005), (0xD6, 0x0001)])),
            Some(true)
        );
        assert_eq!(woke(&with(vec![(0x10, 54)])), None);
    }
}
