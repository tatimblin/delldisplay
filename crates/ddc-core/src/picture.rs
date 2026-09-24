//! Picture, colour and audio as [`Plan`]s and pure decoders. No traffic.
//!
//! - The scoped restores (0x05 levels, 0x08 colour) aren't the factory reset
//!   (0x04, which also resets the OSD language). [`Restore`] gives each its own
//!   warning and its own codes to read back, since a DDC set is unacknowledged.
//! - Dell puts OSD status in bits 14-15 of 0x62 and 0x8D. [`audio::volume`]
//!   masks them off and [`audio::Osd`] decodes them.
//! - On the U4323QE 0x8D is the OSD Speaker switch, not MCCS mute, so
//!   [`audio::switch`] takes its meaning from the profile. Mute works like
//!   DDPM's: 0x62 = 0 with the level remembered on the host ([`MuteMemo`]);
//!   [`plan_unmute`] checks that memo against a live read before restoring it.

use std::time::Duration;

use crate::caps::Capabilities;
use crate::guard::{self, Intent, Snapshot, Verdict};
use crate::plan::Plan;
use crate::vcp::{self, Panel, Vcp};

// Kept here too so `picture::PICTURE_MODE` etc. still resolve.
pub const PICTURE_MODE: u8 = Vcp::PICTURE_MODE;
pub const AUDIO_MUTE: u8 = Vcp::AUDIO_MUTE;
pub const RESTORE_FACTORY: u8 = Vcp::RESTORE_FACTORY;
pub const RESTORE_LEVELS: u8 = Vcp::RESTORE_LEVELS;
pub const RESTORE_COLOR: u8 = Vcp::RESTORE_COLOR;
pub const GAIN_RED: u8 = Vcp::GAIN_RED;
pub const GAIN_GREEN: u8 = Vcp::GAIN_GREEN;
pub const GAIN_BLUE: u8 = Vcp::GAIN_BLUE;

/// Wait before reading a value back. A guess: long enough not to race the
/// scaler; never timed.
pub const READBACK_SETTLE: Duration = Duration::from_millis(300);
/// A restore moves several registers at once, so give it longer. Also a guess.
pub const RESTORE_SETTLE: Duration = Duration::from_millis(1000);

// ---------------------------------------------------------------------------
// Restore defaults
// ---------------------------------------------------------------------------

/// One of the three restore-defaults actions. All are write-only (write 1,
/// then re-read what they should have moved); they differ in what they reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restore {
    /// 0x05: brightness and contrast. Reversible in practice: both are
    /// continuous and their old values can simply be written back.
    Levels,
    /// 0x08: colour. Not reversible from a snapshot: the preset ladder, the
    /// three gains and whatever Custom Color held are not all readable.
    Color,
    /// 0x04: everything, including the OSD language. Not reversible at all.
    Factory,
}

impl Restore {
    pub fn vcp(self) -> u8 {
        match self {
            Restore::Levels => Vcp::RESTORE_LEVELS,
            Restore::Color => Vcp::RESTORE_COLOR,
            Restore::Factory => Vcp::RESTORE_FACTORY,
        }
    }

    /// The CLI verb. Three unrelated words, so a typo can't reach `factory`.
    pub fn verb(self) -> &'static str {
        match self {
            Restore::Levels => "levels",
            Restore::Color => "colour",
            Restore::Factory => "factory",
        }
    }

    /// What this restore actually moves, in one line.
    pub fn scope(self) -> &'static str {
        match self {
            Restore::Levels => "brightness (0x10) and contrast (0x12)",
            Restore::Color => {
                "the colour preset (0x14) and the red/green/blue gains (0x16/0x18/0x1A)"
            }
            Restore::Factory => {
                "every OSD setting on the monitor: colour, geometry, input names, \
                 PiP/PBP arrangement and the menu language"
            }
        }
    }

    /// The warning to show first. Each scope has its own so people don't learn
    /// to skip the factory one.
    pub fn warning(self) -> &'static str {
        match self {
            Restore::Levels => {
                "This resets brightness and contrast to Dell's defaults. It does not \
                 touch colour, inputs or the OSD. Your current levels are captured \
                 first and can be written straight back."
            }
            Restore::Color => {
                "This resets the colour preset and the RGB gains to Dell's defaults. \
                 It does not touch brightness, contrast, inputs or the OSD. It cannot \
                 be undone from a snapshot: Custom Color's contents are not readable \
                 over DDC."
            }
            Restore::Factory => {
                "This resets the entire monitor: every OSD setting, including the \
                 menu language, the input names and the PiP/PBP arrangement. There is \
                 no undo. If you only want picture levels or colour back, use \
                 'restore levels' or 'restore colour' instead."
            }
        }
    }

    /// Codes to read back afterwards to confirm the write landed. Kept short:
    /// each is a `Read` step, and a failed read fails the plan.
    pub fn confirms(self) -> &'static [u8] {
        match self {
            Restore::Levels => &[Vcp::BRIGHTNESS, Vcp::CONTRAST],
            Restore::Color => &[Vcp::COLOR_PRESET],
            Restore::Factory => &[Vcp::BRIGHTNESS, Vcp::CONTRAST, Vcp::COLOR_PRESET],
        }
    }

    /// Codes this restore also moves that aren't read back, so a report can
    /// say what it didn't check.
    fn unchecked(self) -> &'static [u8] {
        match self {
            Restore::Levels => &[],
            Restore::Color => &[Vcp::GAIN_RED, Vcp::GAIN_GREEN, Vcp::GAIN_BLUE],
            Restore::Factory => &[
                Vcp::GAIN_RED,
                Vcp::GAIN_GREEN,
                Vcp::GAIN_BLUE,
                Vcp::INPUT_SOURCE,
                Vcp::PIP_MODE,
            ],
        }
    }

    /// Whether the pre-values captured by the plan are enough to put things
    /// back. True only for [`Restore::Levels`].
    pub fn is_undoable(self) -> bool {
        matches!(self, Restore::Levels)
    }

    /// Accepts the verb plus the spellings a user will reach for.
    pub fn from_name(name: &str) -> Option<Restore> {
        match name.trim().to_ascii_lowercase().as_str() {
            "levels" | "level" | "brightness" | "brightness-contrast" | "picture" => {
                Some(Restore::Levels)
            }
            "colour" | "color" => Some(Restore::Color),
            "factory" | "everything" | "monitor" => Some(Restore::Factory),
            _ => None,
        }
    }
}

/// The restore plan: capture what it will move, write 1, settle, read it back.
///
/// The snapshot is the confirming codes, not the write-only restore code, so
/// [`Plan::undo_from`] (which only restores written codes) correctly yields
/// no undo.
pub fn restore_plan(scope: Restore) -> Plan {
    let mut p = Plan::new(format!("restore {}", scope.verb()))
        .snapshotting(scope.confirms())
        .write(scope.vcp(), 0x0001)
        .dwell(RESTORE_SETTLE);
    for code in scope.confirms() {
        p = p.read(*code);
    }
    p
}

/// The plan that puts brightness and contrast back after `restore levels`.
/// `captured` is `Outcome::snapshot`; `None` if neither level was captured.
pub fn levels_undo(captured: &[(u8, u16)]) -> Option<Plan> {
    let mut p = Plan::new("restore previous levels");
    let mut any = false;
    for code in [Vcp::BRIGHTNESS, Vcp::CONTRAST] {
        if let Some((_, v)) = captured.iter().find(|(c, _)| *c == code) {
            p = p.write(code, *v & 0x00FF);
            any = true;
        }
    }
    any.then_some(p)
}

/// `(code, before, after)` for every confirming code that moved. `before` is
/// `Outcome::snapshot`, `after` is `Outcome::reads`.
pub fn restore_changed(
    panel: &Panel,
    before: &[(u8, u16)],
    after: &[(u8, u16)],
) -> Vec<(u8, u16, u16)> {
    let mut out = Vec::new();
    for (code, was) in before {
        let Some((_, now)) = after.iter().rev().find(|(c, _)| c == code) else {
            continue;
        };
        if !vcp::same_reading(panel, *code, *was, *now) {
            out.push((*code, *was, *now));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Colour preset
// ---------------------------------------------------------------------------

/// VCP 0x14: colour temperature presets plus Custom Color.
///
/// Value names come from the panel profile (Dell's names, not MCCS's: MCCS
/// calls 0x0B "User 1", which is Warm on the U4323QE). Reading and writing
/// 0x14 both work on the U4323QE. The value names are Dell's and haven't been
/// checked against the OSD.
pub mod preset {
    use crate::vcp::{self, Panel, Vcp};

    /// Dell's OSD labels, which the profile doesn't spell.
    pub const ALIASES: [(&str, u8); 5] = [
        ("warm", 0x0B),
        ("cool", 0x08),
        ("custom-color", 0x0C),
        ("custom-colour", 0x0C),
        ("standard", 0x05),
    ];

    /// MCCS values the U4323QE doesn't advertise, so a refusal can name them.
    pub const NOT_HERE: [(&str, u8); 1] = [("srgb", 0x01)];

    pub fn name(panel: &Panel, value: u8) -> Option<&'static str> {
        panel.value_name(Vcp::COLOR_PRESET, value)
    }

    /// Resolve an alias, a profile name, `0xNN` or a decimal number.
    pub fn resolve(panel: &Panel, text: &str) -> Option<u8> {
        let t = text.trim().to_ascii_lowercase();
        if let Some((_, v)) = ALIASES.iter().chain(&NOT_HERE).find(|(n, _)| *n == t) {
            return Some(*v);
        }
        vcp::resolve_value(panel, Vcp::COLOR_PRESET, &t).and_then(|v| u8::try_from(v).ok())
    }

    /// Every name a user may type, for a usage line.
    pub fn choices(panel: &Panel) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = panel
            .lookup(Vcp::COLOR_PRESET)
            .map(|c| c.values.iter().map(|(_, n)| *n).collect())
            .unwrap_or_default();
        out.extend(ALIASES.iter().map(|(n, _)| *n));
        out
    }
}

/// Write a colour preset and read it back.
pub fn preset_plan(panel: &Panel, value: u8) -> Plan {
    let label = preset::name(panel, value).unwrap_or("?");
    Plan::new(format!("colour preset {label}"))
        .snapshotting(&[Vcp::COLOR_PRESET])
        .write(Vcp::COLOR_PRESET, value as u16)
        .dwell(READBACK_SETTLE)
        .verify(Vcp::COLOR_PRESET, value as u16)
}

/// DDPM's preset buttons write 0xDC (picture mode) before 0x14. Inferred from
/// its disassembly, and kept separate from [`preset_plan`] so asking for a
/// colour temperature doesn't quietly change picture mode.
pub fn preset_plan_with_mode(panel: &Panel, value: u8, mode: u8) -> Plan {
    let label = preset::name(panel, value).unwrap_or("?");
    Plan::new(format!("colour preset {label} + picture mode"))
        .snapshotting(&[Vcp::PICTURE_MODE, Vcp::COLOR_PRESET])
        .write(Vcp::PICTURE_MODE, mode as u16)
        .dwell(READBACK_SETTLE)
        .write(Vcp::COLOR_PRESET, value as u16)
        .dwell(READBACK_SETTLE)
        .verify(Vcp::COLOR_PRESET, value as u16)
}

// ---------------------------------------------------------------------------
// Picture mode
// ---------------------------------------------------------------------------

/// VCP 0xDC. The U4323QE advertises one legal value, 0x00 (standard).
///
/// The other names are DDPM's, for other Dell models. They're kept so a
/// refusal can say "movie isn't offered" instead of "0x03 is not legal";
/// [`picture_mode::advertised`] is what a UI should offer.
pub mod picture_mode {
    use crate::vcp::{self, Panel, Vcp};

    pub const STANDARD: u8 = 0x00;

    /// DDPM's names for modes the U4323QE profile doesn't list.
    pub const OTHER_MODELS: [(&str, u8); 5] = [
        ("multimedia", 0x02),
        ("movie", 0x03),
        ("nature", 0x04),
        ("game", 0x05),
        ("sport", 0x06),
    ];

    pub fn name(panel: &Panel, value: u8) -> Option<&'static str> {
        panel.value_name(Vcp::PICTURE_MODE, value).or_else(|| {
            OTHER_MODELS
                .iter()
                .find(|(_, v)| *v == value)
                .map(|(n, _)| *n)
        })
    }

    pub fn resolve(panel: &Panel, text: &str) -> Option<u8> {
        let t = text.trim().to_ascii_lowercase();
        if let Some((_, v)) = OTHER_MODELS.iter().find(|(n, _)| *n == t) {
            return Some(*v);
        }
        vcp::resolve_value(panel, Vcp::PICTURE_MODE, &t).and_then(|v| u8::try_from(v).ok())
    }

    /// The modes this panel accepts, from its capability string.
    pub fn advertised(caps: &super::Capabilities) -> Vec<u8> {
        caps.legal_values(Vcp::PICTURE_MODE)
            .map(|v| v.to_vec())
            .unwrap_or_default()
    }
}

/// Write picture mode and read it back.
pub fn picture_mode_plan(panel: &Panel, value: u8) -> Plan {
    let label = picture_mode::name(panel, value).unwrap_or("?");
    Plan::new(format!("picture mode {label}"))
        .snapshotting(&[Vcp::PICTURE_MODE])
        .write(Vcp::PICTURE_MODE, value as u16)
        .dwell(READBACK_SETTLE)
        .verify(Vcp::PICTURE_MODE, value as u16)
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// Decoders for the two audio registers, both of which carry more than the
/// value they are named for.
pub mod audio {
    use crate::vcp::{Panel, Vcp};

    /// The part of a 0x62 reply that is actually a volume.
    pub const VOLUME_MASK: u16 = 0x00FF;
    /// Bit 14 of both 0x62 and 0x8D: audio is enabled when it's clear.
    pub const OSD_ENABLE_BIT: u16 = 0x4000;
    /// Bit 15 of both 0x62 and 0x8D: the OSD is locked when it's set.
    pub const OSD_LOCK_BIT: u16 = 0x8000;
    /// Bits 0-1 of 0x8D. MCCS calls it mute (1 muted, 2 unmuted), but the
    /// U4323QE uses it for the OSD Speaker switch (0 off, 1 on), so the
    /// meaning comes from the panel profile.
    pub const SWITCH_FIELD: u16 = 0x0003;

    /// The volume, with Dell's status bits masked off.
    pub fn volume(raw: u16) -> u8 {
        (raw & VOLUME_MASK) as u8
    }

    /// The bits [`volume`] masks off, so a report can show them.
    pub fn volume_extra_bits(raw: u16) -> u16 {
        raw & !VOLUME_MASK
    }

    /// The OSD status DDPM derives from 0x62 and 0x8D together. Both words
    /// are needed; DDPM gives up if either read fails.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Osd {
        pub v62: u16,
        pub v8d: u16,
    }

    impl Osd {
        /// Locked needs bit 15 set in both registers (DDPM's rule).
        pub fn locked(self) -> bool {
            self.v62 & OSD_LOCK_BIT != 0 && self.v8d & OSD_LOCK_BIT != 0
        }

        /// Enabled needs bit 14 clear in both registers.
        pub fn audio_enabled(self) -> bool {
            self.v62 & OSD_ENABLE_BIT == 0 && self.v8d & OSD_ENABLE_BIT == 0
        }

        /// True when the two registers disagree on a status bit. DDPM's AND
        /// silently reads that as "no"; neither bit has been seen set on the
        /// U4323QE, so a disagreement is worth printing.
        pub fn bits_disagree(self) -> bool {
            ((self.v62 ^ self.v8d) & (OSD_LOCK_BIT | OSD_ENABLE_BIT)) != 0
        }

        /// DDPM's rendering, e.g. `"OSDUnlocked, OSDEnabled"`.
        pub fn describe(self) -> String {
            format!(
                "{}, {}",
                if self.locked() {
                    "OSDLocked"
                } else {
                    "OSDUnlocked"
                },
                if self.audio_enabled() {
                    "OSDEnabled"
                } else {
                    "OSDDisabled"
                }
            )
        }
    }

    /// 0x8D's switch field and the profile's label for it, e.g.
    /// `(0x01, Some("speaker-on"))` on the U4323QE.
    pub fn switch(panel: &Panel, v8d: u16) -> (u8, Option<&'static str>) {
        let field = (v8d & SWITCH_FIELD) as u8;
        (field, panel.value_name(Vcp::AUDIO_MUTE, field))
    }
}

/// The pre-mute volume, remembered on the host. Muting writes 0x62 = 0, so
/// the panel no longer has the level; unmuting puts this back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuteMemo {
    /// The 0x62 value in force before the mute. Masked to a volume.
    pub level: u8,
}

impl MuteMemo {
    pub fn new(level: u8) -> Self {
        MuteMemo { level }
    }

    /// `"62=54\n"`: the memo file's contents.
    pub fn encode(self) -> String {
        format!("62={}\n", self.level)
    }

    /// Parse [`MuteMemo::encode`] back. Anything else is `None`, so a corrupt
    /// memo reads as "nothing stored", never as level 0.
    pub fn parse(text: &str) -> Option<MuteMemo> {
        let line = text.lines().find(|l| !l.trim().is_empty())?;
        let (code, value) = line.trim().split_once('=')?;
        if !code.trim().eq_ignore_ascii_case("62") {
            return None;
        }
        value.trim().parse::<u8>().ok().map(MuteMemo::new)
    }
}

/// What a mute request should do, given the live volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteDecision {
    /// Remember this level, then write 0x62 = 0.
    Mute(u8),
    /// The panel is already silent; muting would overwrite the memo with 0.
    AlreadySilent,
}

/// Decide a mute from a live 0x62 read.
pub fn plan_mute(live_volume: u8) -> MuteDecision {
    if live_volume == 0 {
        MuteDecision::AlreadySilent
    } else {
        MuteDecision::Mute(live_volume)
    }
}

/// What an unmute request should do, given the live volume and the stored level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmuteDecision {
    /// Safe: the panel is still silent and a level was stored. Write it back.
    Restore(u8),
    /// The panel isn't silent any more (someone changed the volume), so the
    /// stored level is stale. Report it; don't write.
    Moved { live: u8, stored: u8 },
    /// Not silent and nothing stored: nothing to unmute.
    AlreadyAudible(u8),
    /// Silent but no level remembered (muted elsewhere, or the memo was
    /// lost). There's no level to restore.
    NothingStored,
}

/// Check a stored mute level against a live 0x62 read before restoring it.
pub fn plan_unmute(live_volume: u8, stored: Option<u8>) -> UnmuteDecision {
    match (live_volume, stored) {
        (0, Some(level)) if level > 0 => UnmuteDecision::Restore(level),
        (0, _) => UnmuteDecision::NothingStored,
        (live, Some(stored)) => UnmuteDecision::Moved { live, stored },
        (live, None) => UnmuteDecision::AlreadyAudible(live),
    }
}

/// Set the volume and read it back. Unmuting is this with the stored level.
pub fn volume_plan(level: u8) -> Plan {
    Plan::new(format!("volume {level}"))
        .snapshotting(&[Vcp::VOLUME])
        .write(Vcp::VOLUME, level as u16)
        .dwell(READBACK_SETTLE)
        .verify(Vcp::VOLUME, level as u16)
}

/// Mute: 0x62 = 0, then read it back. `level` is the one being remembered,
/// carried in the plan name so a log shows it.
pub fn mute_plan(level: u8) -> Plan {
    Plan::new(format!("mute (was {level})"))
        .snapshotting(&[Vcp::VOLUME])
        .write(Vcp::VOLUME, 0x0000)
        .dwell(READBACK_SETTLE)
        .verify(Vcp::VOLUME, 0x0000)
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// A picture/audio operation, with its guard [`Intent`] and its [`Plan`].
/// Unmuting is `Volume(stored_level)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Preset(u8),
    PresetWithMode { preset: u8, mode: u8 },
    Mode(u8),
    Restore(Restore),
    Volume(u8),
    Mute { level: u8 },
}

impl Op {
    /// The guard intent. These are all plain feature writes, so `Raw`: busy
    /// and capability checks are the preconditions that apply.
    pub fn intent(&self) -> Intent {
        match *self {
            Op::Preset(v) | Op::PresetWithMode { preset: v, .. } => Intent::Raw {
                vcp: Vcp::COLOR_PRESET,
                value: v as u16,
            },
            Op::Mode(v) => Intent::Raw {
                vcp: Vcp::PICTURE_MODE,
                value: v as u16,
            },
            Op::Restore(r) => Intent::Raw {
                vcp: r.vcp(),
                value: 0x0001,
            },
            Op::Volume(v) => Intent::Raw {
                vcp: Vcp::VOLUME,
                value: v as u16,
            },
            Op::Mute { .. } => Intent::Raw {
                vcp: Vcp::VOLUME,
                value: 0x0000,
            },
        }
    }

    /// The plan; `panel` names the values in its title.
    pub fn plan(&self, panel: &Panel) -> Plan {
        match *self {
            Op::Preset(v) => preset_plan(panel, v),
            Op::PresetWithMode { preset, mode } => preset_plan_with_mode(panel, preset, mode),
            Op::Mode(v) => picture_mode_plan(panel, v),
            Op::Restore(r) => restore_plan(r),
            Op::Volume(v) => volume_plan(v),
            Op::Mute { level } => mute_plan(level),
        }
    }
}

/// Rules this area adds on top of [`crate::guard::evaluate`], returned on
/// their own so a caller can print them alongside `Runner::apply`'s.
///
/// `Intent::Raw` only warns about a value outside the advertised list; a named
/// verb should refuse it and show the list. A restore also states its scope.
pub fn preflight(snap: &Snapshot, op: &Op, osd: Option<audio::Osd>) -> Vec<Verdict> {
    let mut out = Vec::new();
    let caps = &snap.caps;

    let enumerated = |code: u8, value: u8, what: &str, out: &mut Vec<Verdict>| {
        let Some(legal) = caps.legal_values(code) else {
            return;
        };
        if legal.is_empty() || legal.contains(&value) {
            return;
        }
        let name: fn(&Panel, u8) -> Option<&'static str> = if code == Vcp::COLOR_PRESET {
            preset::name
        } else {
            picture_mode::name
        };
        let list = legal
            .iter()
            .map(|v| match name(snap.panel, *v) {
                Some(n) => format!("{n} (0x{v:02X})"),
                None => format!("0x{v:02X}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Verdict::Refuse(format!(
            "this panel does not offer {what} 0x{value:02X}; it advertises: {list}"
        )));
    };

    match *op {
        Op::Preset(v) | Op::PresetWithMode { preset: v, .. } => {
            enumerated(Vcp::COLOR_PRESET, v, "colour preset", &mut out);
            if let Op::PresetWithMode { mode, .. } = *op {
                out.push(Verdict::Warn(format!(
                    "also writing picture mode 0x{mode:02X} (0xDC) first, like DDPM's preset \
                     buttons do. That pairing is inferred from its disassembly."
                )));
            }
        }
        Op::Mode(v) => {
            enumerated(Vcp::PICTURE_MODE, v, "picture mode", &mut out);
            out.push(Verdict::Warn(String::from(
                "the U4323QE advertises one legal 0xDC value, so the OSD's other modes \
                 (Movie, Game) aren't reachable over DDC.",
            )));
        }
        Op::Restore(r) => {
            out.push(Verdict::Warn(String::from(r.warning())));
            if !r.unchecked().is_empty() {
                let list = r
                    .unchecked()
                    .iter()
                    .map(|c| format!("0x{c:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                out.push(Verdict::Warn(format!(
                    "the readback covers {} only; {list} will also move and are not checked",
                    r.confirms()
                        .iter()
                        .map(|c| format!("0x{c:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )));
            }
        }
        Op::Volume(_) | Op::Mute { .. } => {
            if let Some(o) = osd {
                if o.locked() {
                    out.push(Verdict::Refuse(String::from(
                        "the OSD is locked (bit 15 set in both 0x62 and 0x8D), the same \
                         condition Dell's software uses to grey out its audio page",
                    )));
                } else if o.bits_disagree() {
                    out.push(Verdict::Warn(format!(
                        "0x62 = 0x{:04X} and 0x8D = 0x{:04X} disagree on the OSD status bits \
                         (14/15). Dell's rule needs both, so this reads as unlocked, but \
                         neither bit has been seen set on the U4323QE, so treat it with care.",
                        o.v62, o.v8d
                    )));
                }
            } else {
                out.push(Verdict::Warn(String::from(
                    "couldn't read the OSD status pair (0x62 + 0x8D), so an OSD lock would \
                     go unnoticed and the write may be dropped",
                )));
            }
        }
    }

    guard::tidy(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::U4323QE;
    use crate::vcp::default_panel;

    fn caps() -> Capabilities {
        Capabilities::parse(U4323QE)
    }

    fn snap() -> Snapshot {
        Snapshot::new(default_panel(), caps())
    }

    // --- restores ---------------------------------------------------------

    #[test]
    fn the_scoped_restores_do_not_inherit_the_factory_warning() {
        let factory = Restore::Factory.warning();
        for narrow in [Restore::Levels, Restore::Color] {
            assert_ne!(narrow.warning(), factory);
            assert!(!narrow.warning().contains("entire monitor"));
        }
        assert!(factory.contains("no undo"));
        // and the narrow ones name what they leave alone
        assert!(Restore::Levels.warning().contains("does not touch colour"));
        assert!(Restore::Color
            .warning()
            .contains("does not touch brightness"));
    }

    #[test]
    fn restore_plan_writes_one_and_reads_back_what_it_moved() {
        let p = restore_plan(Restore::Levels);
        assert_eq!(p.writes(), vec![(0x05, 0x0001)]);
        assert_eq!(p.snapshot, vec![0x10, 0x12]);
        // snapshot then write then two reads
        assert_eq!(p.steps.len(), 4);
        assert!(matches!(p.steps[2], crate::plan::Step::Read { vcp: 0x10 }));
        assert!(matches!(p.steps[3], crate::plan::Step::Read { vcp: 0x12 }));

        let c = restore_plan(Restore::Color);
        assert_eq!(c.writes(), vec![(0x08, 0x0001)]);
        assert_eq!(c.snapshot, vec![0x14]);
    }

    #[test]
    fn a_restore_has_no_undo_but_levels_can_be_written_back() {
        // undo_from only restores codes the plan wrote, and 0x05 is write-only.
        let p = restore_plan(Restore::Levels);
        assert!(p.undo_from(&[(0x10, 54), (0x12, 75)]).is_none());

        // The explicit undo exists only for levels.
        let back = levels_undo(&[(0x10, 0x0036), (0x12, 0x004B)]).unwrap();
        assert_eq!(back.writes(), vec![(0x10, 54), (0x12, 75)]);
        assert!(Restore::Levels.is_undoable());
        assert!(!Restore::Color.is_undoable());
        assert!(!Restore::Factory.is_undoable());
        assert!(levels_undo(&[(0x14, 0x05)]).is_none());
    }

    #[test]
    fn restore_changed_ignores_dells_high_byte() {
        let before = [(0x10, 0x0036), (0x12, 0x004B)];
        let after = [(0x10, 0x004B), (0x12, 0x004B)];
        assert_eq!(
            restore_changed(default_panel(), &before, &after),
            vec![(0x10, 0x0036, 0x004B)]
        );
        // 0x62 status bits in the high byte aren't a change of value
        assert!(restore_changed(default_panel(), &[(0x62, 0x0032)], &[(0x62, 0x8032)]).is_empty());
    }

    #[test]
    fn restore_names_resolve_both_spellings() {
        assert_eq!(Restore::from_name("colour"), Some(Restore::Color));
        assert_eq!(Restore::from_name("color"), Some(Restore::Color));
        assert_eq!(Restore::from_name("LEVELS"), Some(Restore::Levels));
        assert_eq!(Restore::from_name("factory"), Some(Restore::Factory));
        assert_eq!(Restore::from_name("everything else"), None);
    }

    // --- colour preset ----------------------------------------------------

    #[test]
    fn preset_names_are_dells_not_mccss() {
        // MCCS calls 0x0B "User 1"; on the U4323QE it's Warm.
        assert_eq!(preset::name(default_panel(), 0x0B), Some("5700k"));
        assert_eq!(preset::resolve(default_panel(), "warm"), Some(0x0B));
        assert_eq!(preset::resolve(default_panel(), "cool"), Some(0x08));
        assert_eq!(preset::name(default_panel(), 0x08), Some("9300k"));
        assert_eq!(preset::resolve(default_panel(), "custom"), Some(0x0C));
        assert_eq!(
            preset::resolve(default_panel(), "custom-colour"),
            Some(0x0C)
        );
        assert_eq!(preset::resolve(default_panel(), "0x05"), Some(0x05));
        assert_eq!(preset::resolve(default_panel(), "5"), Some(5));
        assert_eq!(preset::resolve(default_panel(), "user1"), None);
    }

    #[test]
    fn preset_choices_cover_the_profile_names_and_aliases() {
        let choices = preset::choices(default_panel());
        for name in ["5000k", "custom", "warm", "cool"] {
            assert!(choices.contains(&name), "{choices:?}");
        }
        assert_eq!(preset::resolve(default_panel(), "srgb"), Some(0x01));
    }

    #[test]
    fn preset_plan_verifies_what_it_wrote() {
        let p = preset_plan(default_panel(), 0x0B);
        assert_eq!(p.writes(), vec![(0x14, 0x000B)]);
        assert!(p.name.contains("5700k"));
        assert!(matches!(
            p.steps.last(),
            Some(crate::plan::Step::Verify {
                vcp: 0x14,
                expect: 0x0B
            })
        ));
        // 0x14 is snapshotted, so a preset change is undoable
        assert_eq!(
            p.undo_from(&[(0x14, 0x05)]).unwrap().writes(),
            vec![(0x14, 0x05)]
        );
    }

    #[test]
    fn pairing_the_mode_with_the_preset_is_opt_in_and_ordered() {
        let p = preset_plan_with_mode(default_panel(), 0x0C, 0x00);
        // DDPM writes the mode first, then the preset.
        assert_eq!(p.writes(), vec![(0xDC, 0x0000), (0x14, 0x000C)]);
        // the plain plan must not touch 0xDC
        assert_eq!(
            preset_plan(default_panel(), 0x0C).written_codes(),
            vec![0x14]
        );
    }

    // --- picture mode -----------------------------------------------------

    #[test]
    fn picture_mode_offers_only_what_the_panel_advertises() {
        assert_eq!(picture_mode::advertised(&caps()), vec![0x00]);
        assert_eq!(
            picture_mode::resolve(default_panel(), "standard"),
            Some(0x00)
        );
        assert_eq!(picture_mode::resolve(default_panel(), "movie"), Some(0x03));
        // resolvable but not offered, so it's refused with the list
        let out = preflight(&snap(), &Op::Mode(0x03), None);
        assert!(out[0].is_refusal());
        assert!(out[0].message().unwrap().contains("standard (0x00)"));
    }

    // --- audio decoders ---------------------------------------------------

    #[test]
    fn volume_is_masked_to_a_byte() {
        assert_eq!(audio::volume(0x0032), 50);
        assert_eq!(audio::volume_extra_bits(0x0032), 0);
        assert_eq!(audio::volume(0xC032), 50);
        assert_eq!(audio::volume_extra_bits(0xC032), 0xC000);
    }

    #[test]
    fn osd_status_needs_both_registers_to_agree() {
        let osd = |v62, v8d| audio::Osd { v62, v8d };
        // The two values read off the U4323QE.
        let o = osd(0x0032, 0x0001);
        assert!(!o.locked());
        assert!(o.audio_enabled());
        assert_eq!(o.describe(), "OSDUnlocked, OSDEnabled");

        // Both set: locked, per DDPM's AND.
        let both = osd(0x8032, 0x8001);
        assert!(both.locked());
        assert!(!both.bits_disagree());

        // One alone isn't a lock, but it is a disagreement.
        let one = osd(0x8032, 0x0001);
        assert!(!one.locked());
        assert!(one.bits_disagree());

        // bit 14 set anywhere disables audio
        assert!(!osd(0x4032, 0x0001).audio_enabled());
        assert!(!osd(0x0032, 0x4001).audio_enabled());
        assert_eq!(osd(0x4032, 0x4001).describe(), "OSDUnlocked, OSDDisabled");
    }

    #[test]
    fn the_8d_switch_is_read_through_the_profile() {
        let u43 = default_panel();
        assert_eq!(audio::switch(u43, 0x0001), (0x01, Some("speaker-on")));
        assert_eq!(audio::switch(u43, 0x0000), (0x00, Some("speaker-off")));
        // status bits don't disturb the field
        assert_eq!(audio::switch(u43, 0xC001), (0x01, Some("speaker-on")));
        // other panels keep MCCS polarity
        let u24 = crate::vcp::for_model("DELL U2421E").unwrap();
        assert_eq!(audio::switch(u24, 0x0001), (0x01, Some("muted")));
        assert_eq!(audio::switch(u24, 0x0002), (0x02, Some("unmuted")));
    }

    // --- mute as DDPM does it --------------------------------------------

    #[test]
    fn muting_remembers_the_level_and_refuses_to_forget_it() {
        assert_eq!(plan_mute(50), MuteDecision::Mute(50));
        // muting an already-silent panel would store 0 and lose the real level
        assert_eq!(plan_mute(0), MuteDecision::AlreadySilent);

        let p = mute_plan(50);
        assert_eq!(p.writes(), vec![(0x62, 0x0000)]);
        assert!(p.name.contains("was 50"));
        // the level is recoverable from the outcome even if the memo is lost
        assert_eq!(
            p.undo_from(&[(0x62, 0x0032)]).unwrap().writes(),
            vec![(0x62, 0x0032)]
        );
    }

    #[test]
    fn a_stale_stored_level_is_reported_not_written() {
        // 0x62 is no longer 0, so someone moved it and the memo is stale.
        assert_eq!(
            plan_unmute(30, Some(50)),
            UnmuteDecision::Moved {
                live: 30,
                stored: 50
            }
        );
        // still silent and a level stored: safe to restore
        assert_eq!(plan_unmute(0, Some(50)), UnmuteDecision::Restore(50));
        // silent but nothing remembered, so don't invent a level
        assert_eq!(plan_unmute(0, None), UnmuteDecision::NothingStored);
        assert_eq!(plan_unmute(0, Some(0)), UnmuteDecision::NothingStored);
        // audible with no memo: there is simply nothing to unmute
        assert_eq!(plan_unmute(30, None), UnmuteDecision::AlreadyAudible(30));
    }

    #[test]
    fn the_memo_round_trips_and_a_corrupt_one_reads_as_nothing() {
        assert_eq!(
            MuteMemo::parse(&MuteMemo::new(54).encode()),
            Some(MuteMemo::new(54))
        );
        assert_eq!(MuteMemo::parse("62=0"), Some(MuteMemo::new(0)));
        // a corrupt memo must never decode as level 0, which is a real value
        assert_eq!(MuteMemo::parse(""), None);
        assert_eq!(MuteMemo::parse("garbage"), None);
        assert_eq!(MuteMemo::parse("10=50"), None);
        assert_eq!(MuteMemo::parse("62=nope"), None);
        assert_eq!(MuteMemo::parse("62=999"), None);
    }

    #[test]
    fn volume_plan_writes_and_verifies_the_level() {
        let p = volume_plan(50);
        assert_eq!(p.writes(), vec![(0x62, 0x0032)]);
        assert!(matches!(
            p.steps.last(),
            Some(crate::plan::Step::Verify {
                vcp: 0x62,
                expect: 50
            })
        ));
    }

    // --- preflight --------------------------------------------------------

    #[test]
    fn an_unadvertised_preset_is_refused_with_the_list() {
        // sRGB (0x01) is in DDPM's vocabulary and not on this panel.
        let out = preflight(&snap(), &Op::Preset(0x01), None);
        assert!(out[0].is_refusal());
        let refused = guard::refusals_of(&out);
        assert_eq!(refused.len(), 1);
        let m = &refused[0];
        assert!(m.contains("5700k (0x0B)"), "{m}");
        assert!(m.contains("custom (0x0C)"), "{m}");
        // An advertised one goes through without a word.
        let ok = preflight(&snap(), &Op::Preset(0x0B), None);
        assert!(ok.is_empty(), "{ok:?}");
    }

    #[test]
    fn a_locked_osd_refuses_an_audio_write_before_anything_is_sent() {
        let locked = audio::Osd {
            v62: 0x8032,
            v8d: 0x8001,
        };
        let out = preflight(&snap(), &Op::Mute { level: 50 }, Some(locked));
        assert!(out[0].is_refusal());
        assert!(out[0].message().unwrap().contains("locked"));

        // unlocked: nothing to say
        let unlocked = audio::Osd {
            v62: 0x0032,
            v8d: 0x0001,
        };
        let fine = preflight(&snap(), &Op::Mute { level: 50 }, Some(unlocked));
        assert!(fine.is_empty(), "{fine:?}");

        // unreadable: a warning, not a silent allow
        let blind = preflight(&snap(), &Op::Volume(20), None);
        assert_eq!(blind.len(), 1);
        assert!(!blind[0].is_refusal());
    }

    #[test]
    fn a_restore_says_its_scope_out_loud_and_names_what_it_cannot_check() {
        let out = preflight(&snap(), &Op::Restore(Restore::Color), None);
        assert!(guard::refusals_of(&out).is_empty());
        let warned = guard::warnings_of(&out);
        assert!(warned[0].contains("does not touch brightness"));
        assert!(warned[1].contains("0x16 0x18 0x1A"));
        // levels checks everything it moves, so there is no "unchecked" line
        let lv = preflight(&snap(), &Op::Restore(Restore::Levels), None);
        assert_eq!(lv.len(), 1);
    }

    #[test]
    fn every_op_offers_an_intent_and_a_plan_that_agree() {
        let ops = [
            Op::Preset(0x0B),
            Op::PresetWithMode {
                preset: 0x0B,
                mode: 0x00,
            },
            Op::Mode(0x00),
            Op::Restore(Restore::Levels),
            Op::Restore(Restore::Color),
            Op::Restore(Restore::Factory),
            Op::Volume(40),
            Op::Mute { level: 40 },
        ];
        for op in ops {
            let plan = op.plan(default_panel());
            let Intent::Raw { vcp, .. } = op.intent() else {
                panic!("{op:?} is not a raw write")
            };
            assert!(
                plan.written_codes().contains(&vcp),
                "{op:?}: intent names 0x{vcp:02X}, plan writes {:02X?}",
                plan.written_codes()
            );
            assert!(!plan.is_read_only(), "{op:?} writes nothing");
            // and every code it writes is one this panel advertises
            for c in plan.written_codes() {
                assert!(caps().supports(c), "{op:?} writes unadvertised 0x{c:02X}");
            }
        }
    }
}
