//! Snapshot a panel's VCP state to JSON and replay it.
//!
//! Export ([`StateSnapshot`]) is the `status` read loop written down with the
//! monitor's key attached. Import ([`import_plan`]) turns a snapshot back into
//! a [`Plan`]. Only [`IMPORT_ALLOWED`] codes are written, so a code nobody has
//! thought about is skipped, and a snapshot from another panel is refused,
//! since input codes, KVM association and PiP layout don't transfer.
//!
//! Pure: no clock, no filesystem, no I2C.

use crate::identity::{match_keys, KeyMatch, MonitorKey};
use crate::plan::{Plan, LAYOUT_SETTLE};
use crate::vcp::{self, Panel, Vcp};

// ---------------------------------------------------------------------------
// Export / import
// ---------------------------------------------------------------------------

/// Codes an export reads. Generous, since reading is harmless and a snapshot
/// doubles as a diagnostic.
pub const EXPORT_CODES: &[u8] = &[
    0x10, 0x12, 0x14, 0x16, 0x18, 0x1A, // picture
    0x60, 0x62, 0x63, 0x8D, // input and audio
    0xCC, 0xD6, 0xDC, 0xDF, // osd language, power mode, picture mode, mccs
    0xC8, 0xC9, 0xFD, // firmware, recorded but never replayed
    0xE0, 0xE1, 0xE2, // powernap pair, preset mode
    0xE7, 0xE8, 0xE9, 0xEE, // kvm, sub-source, layout, port inventory
    0xF1, 0xF2, // feature and status words
];

/// Codes an import may write. Everything else is skipped and reported,
/// including:
///
/// - `0x04` restore-factory: a snapshot isn't a request to wipe the panel.
/// - `0xC8`, `0xC9`, `0xFD`, `0xF1`, `0xF2`: identity and status, read-only.
/// - `0xFE`: meaning unknown, and it sits next to the firmware codes.
/// - `0xD6`, `0xE0`, `0xE1`: power and PowerNap can blank the panel.
/// - `0xE5`, `0xE7`: action registers that don't read back what they did.
/// - `0xEE`: the port inventory is fixed by the hardware.
pub const IMPORT_ALLOWED: &[u8] = &[
    0x10, 0x12, 0x14, 0x16, 0x18, 0x1A, 0x60, 0x62, 0x63, 0x8D, 0xCC, 0xDC, 0xE2, 0xE8, 0xE9,
];

/// The layout group, in the order a layout change must be applied (the same
/// as [`crate::pxp::apply_plan`]). Everything else is written after it.
const LAYOUT_GROUP: [u8; 3] = [Vcp::PIP_MODE, Vcp::INPUT_SOURCE, Vcp::PIP_SUB_SOURCE];

/// Everything an export writes down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub monitor: MonitorKey,
    /// Unix seconds, supplied by the caller.
    pub taken_unix: u64,
    /// `(code, value)` in read order.
    pub values: Vec<(u8, u16)>,
    /// Codes that were asked for and didn't answer, so an import can say the
    /// snapshot never had them rather than implying a default.
    pub gaps: Vec<u8>,
}

impl StateSnapshot {
    /// Build from the `(values, gaps)` pair `Runner::capture` returns.
    pub fn from_capture(
        monitor: MonitorKey,
        taken_unix: u64,
        values: &[(u8, u16)],
        gaps: &[u8],
    ) -> Self {
        StateSnapshot {
            monitor,
            taken_unix,
            values: values.to_vec(),
            gaps: gaps.to_vec(),
        }
    }

    pub fn value_of(&self, code: u8) -> Option<u16> {
        self.values
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v)| *v)
    }
}

/// Why a code in a snapshot wasn't written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Not in [`IMPORT_ALLOWED`].
    NotReplayable,
    /// Left out by the caller's filter ([`ImportOptions::only`]).
    Filtered,
    /// The panel already holds this value.
    AlreadySet,
}

impl Skip {
    pub fn reason(self) -> &'static str {
        match self {
            Skip::NotReplayable => "not on the import allow-list",
            Skip::Filtered => "not in the requested codes",
            Skip::AlreadySet => "already at this value",
        }
    }
}

/// Caller choices for an import.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Replay onto a panel whose serial differs or is unknown.
    pub allow_other_serial: bool,
    /// Restrict to these codes.
    pub only: Option<Vec<u8>>,
    /// Current panel readings. Codes already at the snapshot's value are
    /// skipped, so a correct layout isn't rewritten and re-synced.
    pub current: Vec<(u8, u16)>,
}

/// The result of planning an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    pub plan: Plan,
    /// Codes not written, with the reason, in snapshot order.
    pub skipped: Vec<(u8, Skip)>,
    /// Non-fatal caveats worth printing.
    pub warnings: Vec<String>,
    /// How the snapshot's key compared with the panel's.
    pub matched: KeyMatch,
}

impl ImportPlan {
    /// True when the import would write nothing.
    pub fn is_empty(&self) -> bool {
        self.plan.is_read_only()
    }
}

/// Turn a snapshot into a plan, or refuse. `Err` means nothing should be
/// issued.
///
/// The layout group goes first, in 0xE9, 0x60, 0xE8 order with an await after
/// each layout write, then the picture and audio codes. On the U4323QE each
/// input has its own picture settings, so brightness written before the input
/// switch lands on the input being left.
pub fn import_plan(
    snap: &StateSnapshot,
    panel: &MonitorKey,
    profile: &Panel,
    opts: &ImportOptions,
) -> Result<ImportPlan, String> {
    let matched = match_keys(&snap.monitor, panel);
    let mut warnings = Vec::new();

    match matched {
        KeyMatch::Different => {
            return Err(format!(
                "this snapshot was taken on {} and this panel is {}; a different model's \
                 input codes, KVM association and PiP layout don't transfer",
                snap.monitor, panel
            ));
        }
        KeyMatch::SameModel if !opts.allow_other_serial => {
            return Err(format!(
                "same model but a different panel: the snapshot is from serial {}, this is {}. \
                 Re-run with the override if you meant to copy settings between two monitors",
                snap.monitor.serial, panel.serial
            ));
        }
        KeyMatch::Unknown if !opts.allow_other_serial => {
            return Err(format!(
                "cannot confirm this is the same panel: {} has no serial to compare \
                 (snapshot {}, panel {}). Re-run with the override to replay anyway",
                if snap.monitor.serial.is_empty() {
                    "the snapshot"
                } else {
                    "this panel"
                },
                snap.monitor,
                panel
            ));
        }
        KeyMatch::SameModel => warnings.push(format!(
            "replaying a snapshot from serial {} onto serial {}: same model, different panel",
            snap.monitor.serial, panel.serial
        )),
        KeyMatch::Unknown => warnings.push(String::from(
            "no serial to compare, so this may not be the panel the snapshot came from",
        )),
        KeyMatch::Exact => {}
    }

    let mut skipped: Vec<(u8, Skip)> = Vec::new();
    let mut writes: Vec<(u8, u16)> = Vec::new();

    for &(code, value) in &snap.values {
        let skip = if !IMPORT_ALLOWED.contains(&code) {
            Some(Skip::NotReplayable)
        } else if opts.only.as_ref().is_some_and(|only| !only.contains(&code)) {
            Some(Skip::Filtered)
        } else if opts
            .current
            .iter()
            .any(|&(c, now)| c == code && vcp::same_reading(profile, code, now, value))
        {
            Some(Skip::AlreadySet)
        } else {
            None
        };
        match skip {
            Some(why) => skipped.push((code, why)),
            None => writes.push((code, value)),
        }
    }

    if !snap.gaps.is_empty() {
        warnings.push(format!(
            "the snapshot has no value for {}, so those codes are left alone",
            hex_list(&snap.gaps)
        ));
    }

    // Layout group first, in its own order; everything else keeps snapshot order.
    let group_pos = |code: u8| {
        LAYOUT_GROUP
            .iter()
            .position(|c| *c == code)
            .unwrap_or(LAYOUT_GROUP.len())
    };
    writes.sort_by_key(|(code, _)| group_pos(*code));

    let mut plan = Plan::new(format!("import {}", snap.monitor));
    for (i, &(code, value)) in writes.iter().enumerate() {
        plan = plan.write(code, value);
        if LAYOUT_GROUP.contains(&code) && i + 1 < writes.len() {
            plan = plan.await_ready(code, LAYOUT_SETTLE);
        }
    }

    Ok(ImportPlan {
        plan: plan.snapshot_writes(),
        skipped,
        warnings,
        matched,
    })
}

// ---------------------------------------------------------------------------
// Serialisation
// ---------------------------------------------------------------------------

/// `kind` field of an exported snapshot file.
pub const SNAPSHOT_KIND: &str = "delldisplay.monitor-snapshot";
/// Format version of the snapshot file.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ParseError {}

impl StateSnapshot {
    /// Serialise to JSON. `hex` and `name` are for people reading the file;
    /// only `code` and `value` are read back.
    pub fn to_json(&self, panel: &Panel) -> String {
        let values: Vec<String> = self
            .values
            .iter()
            .map(|&(code, value)| {
                let name = panel.lookup(code).map(|c| c.name).unwrap_or("unknown");
                format!(
                    "{{\"code\":{code},\"hex\":\"0x{code:02X}\",\"name\":{},\"value\":{value}}}",
                    quote(name),
                )
            })
            .collect();
        let gaps: Vec<String> = self.gaps.iter().map(|c| c.to_string()).collect();
        format!(
            "{{\"kind\":{},\"version\":{},\"monitor\":{{\"model\":{},\"serial\":{}}},\
             \"taken_unix\":{},\"values\":[{}],\"gaps\":[{}]}}",
            quote(SNAPSHOT_KIND),
            FORMAT_VERSION,
            quote(&self.monitor.model),
            quote(&self.monitor.serial),
            self.taken_unix,
            values.join(","),
            gaps.join(",")
        )
    }

    pub fn from_json(text: &str) -> Result<StateSnapshot, ParseError> {
        let err = |m: String| ParseError(m);
        let v = json::parse(text).map_err(err)?;
        match v.get("kind").and_then(json::Val::as_str) {
            Some(SNAPSHOT_KIND) => {}
            Some(k) => return Err(err(format!("this is a {k} file, expected {SNAPSHOT_KIND}"))),
            None => return Err(err(format!("not a {SNAPSHOT_KIND} file: no 'kind' field"))),
        }
        let monitor = v
            .get("monitor")
            .ok_or_else(|| err(String::from("no monitor identity in this file")))?;
        let model = monitor
            .get("model")
            .and_then(json::Val::as_str)
            .unwrap_or("");
        if model.is_empty() {
            return Err(err(String::from("monitor identity has no model")));
        }
        let serial = monitor
            .get("serial")
            .and_then(json::Val::as_str)
            .unwrap_or("");
        let monitor = MonitorKey::new(model, serial);
        let taken_unix = v.get("taken_unix").and_then(json::Val::as_u64).unwrap_or(0);

        let mut values = Vec::new();
        for item in v.get("values").and_then(json::Val::as_arr).unwrap_or(&[]) {
            let field = |name: &str| {
                item.get(name)
                    .and_then(json::Val::as_u64)
                    .ok_or_else(|| err(format!("a value entry has no numeric '{name}'")))
            };
            let (code, value) = (field("code")?, field("value")?);
            if code > 0xFF || value > 0xFFFF {
                return Err(err(format!(
                    "value entry out of range: code {code}, value {value}"
                )));
            }
            values.push((code as u8, value as u16));
        }

        let mut gaps = Vec::new();
        for g in v.get("gaps").and_then(json::Val::as_arr).unwrap_or(&[]) {
            match g.as_u64() {
                Some(c) if c <= 0xFF => gaps.push(c as u8),
                _ => return Err(err(format!("gap entry is not a VCP code: {g:?}"))),
            }
        }
        Ok(StateSnapshot {
            monitor,
            taken_unix,
            values,
            gaps,
        })
    }
}

// Small text helpers shared by the writer and the warnings above.

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn hex_list(codes: &[u8]) -> String {
    codes
        .iter()
        .map(|c| format!("0x{c:02X}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The smallest JSON reader that handles a hand-edited version of what
/// [`StateSnapshot::to_json`] writes: objects, arrays, strings with the usual
/// escapes, numbers, booleans and null.
mod json {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Val {
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Arr(Vec<Val>),
        Obj(Vec<(String, Val)>),
    }

    impl Val {
        pub fn get(&self, key: &str) -> Option<&Val> {
            match self {
                Val::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Val::Str(s) => Some(s),
                _ => None,
            }
        }
        pub fn as_u64(&self) -> Option<u64> {
            match self {
                Val::Num(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
                _ => None,
            }
        }
        pub fn as_arr(&self) -> Option<&[Val]> {
            match self {
                Val::Arr(v) => Some(v),
                _ => None,
            }
        }
    }

    pub fn parse(text: &str) -> Result<Val, String> {
        let b = text.as_bytes();
        let mut i = 0usize;
        let v = value(b, &mut i)?;
        skip_ws(b, &mut i);
        if i != b.len() {
            return Err(format!("trailing text at byte {i}"));
        }
        Ok(v)
    }

    fn skip_ws(b: &[u8], i: &mut usize) {
        while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
            *i += 1;
        }
    }

    fn value(b: &[u8], i: &mut usize) -> Result<Val, String> {
        skip_ws(b, i);
        match b.get(*i) {
            None => Err(String::from("unexpected end of input")),
            Some(b'{') => object(b, i),
            Some(b'[') => array(b, i),
            Some(b'"') => string(b, i).map(Val::Str),
            Some(b't') => lit(b, i, "true", Val::Bool(true)),
            Some(b'f') => lit(b, i, "false", Val::Bool(false)),
            Some(b'n') => lit(b, i, "null", Val::Null),
            Some(_) => number(b, i),
        }
    }

    fn lit(b: &[u8], i: &mut usize, word: &str, v: Val) -> Result<Val, String> {
        if b[*i..].starts_with(word.as_bytes()) {
            *i += word.len();
            Ok(v)
        } else {
            Err(format!("expected {word} at byte {i}"))
        }
    }

    fn object(b: &[u8], i: &mut usize) -> Result<Val, String> {
        *i += 1; // '{'
        let mut out = Vec::new();
        skip_ws(b, i);
        if b.get(*i) == Some(&b'}') {
            *i += 1;
            return Ok(Val::Obj(out));
        }
        loop {
            skip_ws(b, i);
            let k = string(b, i)?;
            skip_ws(b, i);
            if b.get(*i) != Some(&b':') {
                return Err(format!("expected ':' at byte {i}"));
            }
            *i += 1;
            let v = value(b, i)?;
            out.push((k, v));
            skip_ws(b, i);
            match b.get(*i) {
                Some(b',') => *i += 1,
                Some(b'}') => {
                    *i += 1;
                    return Ok(Val::Obj(out));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {i}")),
            }
        }
    }

    fn array(b: &[u8], i: &mut usize) -> Result<Val, String> {
        *i += 1; // '['
        let mut out = Vec::new();
        skip_ws(b, i);
        if b.get(*i) == Some(&b']') {
            *i += 1;
            return Ok(Val::Arr(out));
        }
        loop {
            out.push(value(b, i)?);
            skip_ws(b, i);
            match b.get(*i) {
                Some(b',') => *i += 1,
                Some(b']') => {
                    *i += 1;
                    return Ok(Val::Arr(out));
                }
                _ => return Err(format!("expected ',' or ']' at byte {i}")),
            }
        }
    }

    fn string(b: &[u8], i: &mut usize) -> Result<String, String> {
        if b.get(*i) != Some(&b'"') {
            return Err(format!("expected a string at byte {i}"));
        }
        *i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = b.get(*i).copied() else {
                return Err(String::from("unterminated string"));
            };
            *i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(e) = b.get(*i).copied() else {
                        return Err(String::from("unterminated escape"));
                    };
                    *i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = b
                                .get(*i..*i + 4)
                                .ok_or_else(|| String::from("short \\u escape"))?;
                            let n = u32::from_str_radix(
                                std::str::from_utf8(hex)
                                    .map_err(|_| String::from("bad \\u escape"))?,
                                16,
                            )
                            .map_err(|_| String::from("bad \\u escape"))?;
                            *i += 4;
                            out.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                        }
                        other => return Err(format!("unknown escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Collect the raw byte; multi-byte UTF-8 passes through
                    // unchanged because we rebuild from the original slice.
                    let start = *i - 1;
                    let mut end = *i;
                    while end < b.len() && b[end] & 0xC0 == 0x80 {
                        end += 1;
                    }
                    let s = std::str::from_utf8(&b[start..end])
                        .map_err(|_| String::from("invalid UTF-8 in string"))?;
                    out.push_str(s);
                    *i = end;
                }
            }
        }
    }

    fn number(b: &[u8], i: &mut usize) -> Result<Val, String> {
        let start = *i;
        if b.get(*i) == Some(&b'-') {
            *i += 1;
        }
        while *i < b.len()
            && (b[*i].is_ascii_digit() || matches!(b[*i], b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            *i += 1;
        }
        let s = std::str::from_utf8(&b[start..*i]).map_err(|_| String::from("bad number"))?;
        s.parse::<f64>()
            .map(Val::Num)
            .map_err(|_| format!("bad number '{s}'"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Step;
    use crate::vcp::default_panel;

    fn here() -> MonitorKey {
        MonitorKey::new("U4323QE", "ABC123")
    }

    fn snap(values: &[(u8, u16)]) -> StateSnapshot {
        StateSnapshot::from_capture(here(), 1_700_000_000, values, &[])
    }

    // ---- import guards ---------------------------------------------------

    #[test]
    fn destructive_and_read_only_codes_are_not_on_the_allow_list() {
        for code in [
            0x04u8, 0xC8, 0xC9, 0xFD, 0xF1, 0xF2, 0xFE, 0xD6, 0xE0, 0xE1, 0xE5, 0xE7,
        ] {
            assert!(!IMPORT_ALLOWED.contains(&code), "0x{code:02X}");
        }
    }

    #[test]
    fn import_refuses_a_snapshot_from_a_different_model() {
        let s = snap(&[(0x10, 50)]);
        let other = MonitorKey::new("U2723QE", "ABC123");
        let err = import_plan(&s, &other, default_panel(), &ImportOptions::default()).unwrap_err();
        assert!(err.contains("U4323QE"), "{err}");
        assert!(err.contains("U2723QE"), "{err}");
    }

    #[test]
    fn import_refuses_a_different_serial_unless_overridden() {
        let s = snap(&[(0x10, 50)]);
        let sibling = MonitorKey::new("U4323QE", "ZZZ999");
        assert!(import_plan(&s, &sibling, default_panel(), &ImportOptions::default()).is_err());

        let opts = ImportOptions {
            allow_other_serial: true,
            ..Default::default()
        };
        let ok = import_plan(&s, &sibling, default_panel(), &opts).unwrap();
        assert_eq!(ok.matched, KeyMatch::SameModel);
        assert_eq!(ok.plan.writes(), vec![(0x10, 50)]);
        assert!(
            ok.warnings.iter().any(|w| w.contains("different panel")),
            "{:?}",
            ok.warnings
        );
    }

    #[test]
    fn import_refuses_when_the_serial_is_unknown() {
        let s = snap(&[(0x10, 50)]);
        let nameless = MonitorKey::new("U4323QE", "");
        let err =
            import_plan(&s, &nameless, default_panel(), &ImportOptions::default()).unwrap_err();
        assert!(err.contains("cannot confirm"), "{err}");
    }

    #[test]
    fn unlisted_codes_are_skipped_with_a_reason() {
        let s = snap(&[
            (0x10, 50),     // allowed
            (0x04, 1),      // restore-factory
            (0xC8, 5),      // identity
            (0xF1, 0xC12B), // feature word
            (0xD6, 1),      // power
            (0xE7, 0x0240), // action register
        ]);
        let p = import_plan(&s, &here(), default_panel(), &ImportOptions::default()).unwrap();
        assert_eq!(p.plan.writes(), vec![(0x10, 50)]);
        let skipped: Vec<u8> = p.skipped.iter().map(|(c, _)| *c).collect();
        assert_eq!(skipped, vec![0x04, 0xC8, 0xF1, 0xD6, 0xE7]);
        assert!(p.skipped.iter().all(|(_, why)| *why == Skip::NotReplayable));
    }

    #[test]
    fn the_layout_group_is_written_first_in_its_own_order() {
        // Snapshot order is scrambled: 0xE8 first, 0xE9 last.
        let s = snap(&[(0xE8, 0x6DFB), (0x10, 50), (0x60, 0x1B), (0xE9, 0x24)]);
        let p = import_plan(&s, &here(), default_panel(), &ImportOptions::default()).unwrap();
        assert_eq!(
            p.plan.writes(),
            vec![(0xE9, 0x24), (0x60, 0x1B), (0xE8, 0x6DFB), (0x10, 50)]
        );
        let awaits: Vec<u8> = p
            .plan
            .steps
            .iter()
            .filter_map(|s| match *s {
                Step::AwaitReady { vcp, timeout } => {
                    assert_eq!(timeout, LAYOUT_SETTLE);
                    Some(vcp)
                }
                _ => None,
            })
            .collect();
        assert_eq!(awaits, vec![0xE9, 0x60, 0xE8]);
        assert!(p.plan.total_dwell().is_zero(), "awaits, not blind dwells");
    }

    #[test]
    fn a_picture_only_import_does_not_wait() {
        let s = snap(&[(0x10, 50), (0x12, 70)]);
        let p = import_plan(&s, &here(), default_panel(), &ImportOptions::default()).unwrap();
        assert_eq!(p.plan.steps.len(), 2);
        assert_eq!(p.plan.writes(), vec![(0x10, 50), (0x12, 70)]);
    }

    #[test]
    fn an_import_snapshots_what_it_writes_so_it_can_be_undone() {
        let s = snap(&[(0x10, 50), (0xE9, 0x24)]);
        let p = import_plan(&s, &here(), default_panel(), &ImportOptions::default()).unwrap();
        assert_eq!(p.plan.snapshot, p.plan.written_codes());
        // Undo runs in reverse write order.
        let undo = p.plan.undo_from(&[(0x10, 30), (0xE9, 0x00)]).unwrap();
        assert_eq!(undo.writes(), vec![(0x10, 30), (0xE9, 0x00)]);
    }

    #[test]
    fn values_already_set_are_not_rewritten() {
        let s = snap(&[(0x10, 50), (0xE9, 0x24), (0x60, 0x1B)]);
        let opts = ImportOptions {
            current: vec![(0xE9, 0x0024), (0x60, 0x1B1B), (0x10, 30)],
            ..Default::default()
        };
        let p = import_plan(&s, &here(), default_panel(), &opts).unwrap();
        assert_eq!(p.plan.writes(), vec![(0x10, 50)]);
        assert!(p.skipped.contains(&(0xE9, Skip::AlreadySet)));
        // 0x60 reads 0x1B1B for a written 0x1B.
        assert!(p.skipped.contains(&(0x60, Skip::AlreadySet)));
    }

    #[test]
    fn only_filters_the_import() {
        let s = snap(&[(0x10, 50), (0x12, 70)]);
        let opts = ImportOptions {
            only: Some(vec![0x12]),
            ..Default::default()
        };
        let p = import_plan(&s, &here(), default_panel(), &opts).unwrap();
        assert_eq!(p.plan.writes(), vec![(0x12, 70)]);
        assert!(p.skipped.contains(&(0x10, Skip::Filtered)));
    }

    #[test]
    fn gaps_in_the_snapshot_are_reported() {
        let mut s = snap(&[(0x10, 50)]);
        s.gaps = vec![0x62, 0x8D];
        let p = import_plan(&s, &here(), default_panel(), &ImportOptions::default()).unwrap();
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("0x62") && w.contains("0x8D")),
            "{:?}",
            p.warnings
        );
    }

    // ---- serialisation ---------------------------------------------------

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let mut s = snap(&[(0x10, 50), (0xE9, 0x0024)]);
        s.gaps = vec![0x62];
        let back = StateSnapshot::from_json(&s.to_json(default_panel())).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn the_wrong_kind_of_file_is_an_error() {
        let e = StateSnapshot::from_json(r#"{"kind":"something-else"}"#).unwrap_err();
        assert!(e.0.contains("expected"), "{e}");
        assert!(StateSnapshot::from_json("{oh no").is_err());
    }

    #[test]
    fn hand_edited_json_still_parses() {
        // Whitespace, reordered keys, extra fields a human added.
        let text = r#"
        {
          "version": 1,
          "kind": "delldisplay.monitor-snapshot",
          "why": "before the firmware update",
          "monitor": { "serial": "ABC123", "model": "U4323QE" },
          "taken_unix": 1700000000,
          "values": [
            { "code": 16, "hex": "0x10", "name": "brightness", "value": 50 }
          ]
        }"#;
        let s = StateSnapshot::from_json(text).unwrap();
        assert_eq!(s.monitor, here());
        assert_eq!(s.value_of(0x10), Some(50));
    }

    #[test]
    fn a_snapshot_missing_the_model_is_rejected() {
        let text = r#"{"kind":"delldisplay.monitor-snapshot","version":1,
                       "monitor":{"serial":"X"},"values":[]}"#;
        assert!(StateSnapshot::from_json(text).is_err());
    }

    #[test]
    fn an_out_of_range_gap_is_rejected() {
        let text = r#"{"kind":"delldisplay.monitor-snapshot","version":1,
                       "monitor":{"model":"U4323QE","serial":"X"},"values":[],"gaps":[98,300]}"#;
        assert!(StateSnapshot::from_json(text).is_err());
    }
}
