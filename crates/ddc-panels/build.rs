//! Generates Rust from `mccs.toml` and `profiles/*.toml`. See profiles/README.md.
use std::{collections::BTreeMap, env, fmt::Write as _, fs, path::Path};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    panel: PanelToml,
    #[serde(default)]
    quirks: QuirksToml,
    #[serde(default)]
    code: Vec<Code>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelToml {
    models: Vec<String>,
    #[serde(default)]
    mccs: String,
    #[serde(default)]
    default: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct QuirksToml {
    double_write: bool,
    type_byte_reliable: bool,
    refusal_result: u8,
    enumerated: Vec<u8>,
    slow_write_codes: Vec<u8>,
    slow_write_ms: u64,
    normal_write_ms: u64,
}

impl Default for QuirksToml {
    fn default() -> Self {
        QuirksToml {
            double_write: true,
            type_byte_reliable: false,
            refusal_result: 0x01,
            enumerated: vec![0x14, 0x60, 0xCC, 0xD6, 0xDC, 0xE2, 0xE9],
            slow_write_codes: vec![0xE0, 0xE1, 0xD6],
            slow_write_ms: 150,
            normal_write_ms: 60,
        }
    }
}

/// The MCCS baseline: codes only, every one of them `spec`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    code: Vec<Code>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Code {
    vcp: u8,
    name: String,
    provenance: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    values: String,
}

fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn parse_u8(s: &str) -> Option<u8> {
    match s.strip_prefix("0x") {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

/// `"0x0F=dp1, 0x11=hdmi1"` -> pairs, panicking on anything malformed.
fn parse_values(file: &str, c: &Code) -> Vec<(u8, String)> {
    let mut out: Vec<(u8, String)> = Vec::new();
    for pair in c.values.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let parsed = pair
            .split_once('=')
            .and_then(|(k, v)| Some((parse_u8(k.trim())?, v)));
        let Some((k, v)) = parsed else {
            panic!("{file}: vcp {:#04x}: bad value entry {pair:?}", c.vcp);
        };
        let v = v.trim().to_string();
        if !is_label(&v) {
            panic!(
                "{file}: vcp {:#04x}: value label {v:?} must be lowercase a-z, 0-9, -",
                c.vcp
            );
        }
        if out.iter().any(|(ok, ov)| *ok == k || *ov == v) {
            panic!("{file}: vcp {:#04x}: duplicate value {pair:?}", c.vcp);
        }
        out.push((k, v));
    }
    out
}

fn provenance(file: &str, c: &Code) -> &'static str {
    match c.provenance.as_str() {
        "observed" => "Observed",
        "inferred" => "Inferred",
        "spec" => "Spec",
        "unknown" => "Unknown",
        p => panic!("{file}: vcp {:#04x}: unknown provenance {p:?}", c.vcp),
    }
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Checks a code list and emits it as a `&[CodeInfo]` static.
fn emit_codes(
    out: &mut String,
    file: &str,
    ident: &str,
    codes: &mut [Code],
    names: &mut BTreeMap<String, (u8, String)>,
) {
    codes.sort_by_key(|c| c.vcp);
    for w in codes.windows(2) {
        if w[0].vcp == w[1].vcp {
            panic!("{file}: vcp {:#04x} listed twice", w[0].vcp);
        }
    }
    writeln!(out, "static {ident}: &[CodeInfo] = &[").unwrap();
    for c in codes.iter() {
        if !is_label(&c.name) {
            panic!(
                "{file}: vcp {:#04x}: name {:?} must be lowercase a-z, 0-9, -",
                c.vcp, c.name
            );
        }
        // A name has to mean the same register on every panel, or `set <name>` would
        // write a different code depending on which monitor is attached.
        match names.get(&c.name) {
            Some((vcp, other)) if *vcp != c.vcp => panic!(
                "{file}: name {:?} is vcp {:#04x} here but {vcp:#04x} in {other}",
                c.name, c.vcp
            ),
            Some(_) => {}
            None => {
                names.insert(c.name.clone(), (c.vcp, file.to_string()));
            }
        }
        let prov = provenance(file, c);
        let vals: Vec<String> = parse_values(file, c)
            .iter()
            .map(|(k, v)| format!("({k:#04x}, {v:?})"))
            .collect();
        writeln!(
            out,
            "    CodeInfo {{ vcp: {:#04x}, name: {:?}, provenance: Provenance::{prov}, note: {:?}, values: &[{}] }},",
            c.vcp,
            c.name,
            c.note.trim(),
            vals.join(", ")
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = root.join("profiles");
    println!("cargo:rerun-if-changed=profiles");

    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("profiles/ missing")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no profiles to compile");

    let mut out = String::from("// @generated from mccs.toml and profiles/*.toml; do not edit.\n");
    let mut names = BTreeMap::new();

    let mut baseline: Baseline = read(&root.join("mccs.toml"));
    for c in &baseline.code {
        assert!(
            c.provenance == "spec",
            "mccs.toml: vcp {:#04x}: baseline entries must be provenance \"spec\"",
            c.vcp
        );
    }
    emit_codes(
        &mut out,
        "mccs.toml",
        "BASELINE_CODES",
        &mut baseline.code,
        &mut names,
    );

    let mut panels = String::new();
    let mut seen_models: BTreeMap<String, String> = BTreeMap::new();
    let mut defaults = Vec::new();
    for (i, path) in files.iter().enumerate() {
        let file = path.file_name().unwrap().to_str().unwrap().to_string();
        let mut p: Profile = read(path);
        if p.panel.models.is_empty() {
            panic!("{file}: [panel] models is empty");
        }
        for m in &p.panel.models {
            if m.trim().is_empty() {
                panic!("{file}: empty model name");
            }
            if let Some(other) = seen_models.insert(m.to_ascii_uppercase(), file.clone()) {
                panic!("{file}: model {m:?} is also claimed by {other}");
            }
        }
        if p.panel.default {
            defaults.push(i);
        }
        let ident = format!("CODES_{i}");
        emit_codes(&mut out, &file, &ident, &mut p.code, &mut names);

        let q = &p.quirks;
        writeln!(
            panels,
            "    Panel {{ model: {:?}, models: &{:?}, mccs: {:?}, codes: {ident}, quirks: Quirks {{ \
             double_write: {}, type_byte_reliable: {}, refusal_result: {:#04x}, \
             enumerated: &{:?}, slow_write_codes: &{:?}, slow_write_ms: {}, normal_write_ms: {} }} }},",
            p.panel.models[0], p.panel.models, p.panel.mccs,
            q.double_write, q.type_byte_reliable, q.refusal_result,
            q.enumerated, q.slow_write_codes, q.slow_write_ms, q.normal_write_ms
        )
        .unwrap();
    }

    let [default] = defaults[..] else {
        panic!(
            "exactly one profile must set `default = true` in [panel], found {}",
            defaults.len()
        );
    };

    writeln!(out, "pub static PROFILES: &[Panel] = &[\n{panels}];").unwrap();
    writeln!(out, "const DEFAULT_INDEX: usize = {default};").unwrap();
    writeln!(
        out,
        "#[cfg(test)]\nconst MODEL_COUNT: usize = {};",
        seen_models.len()
    )
    .unwrap();

    let dest = Path::new(&env::var("OUT_DIR").unwrap()).join("panel_data.rs");
    fs::write(dest, out).unwrap();
}
