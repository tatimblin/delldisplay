# Contributing

## Build and check

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI runs the same, plus `cargo doc` with warnings denied. Tests run against fake
panels and captured frames. None of them touch real hardware.

## Commits and releases

PR titles follow [Conventional Commits](https://www.conventionalcommits.org)
(`feat: ...`, `fix: ...`, `docs: ...`), with a lowercase subject; CI checks
this. Squash-merge PRs so the title becomes the commit on `main`.

Releases are automated. On every push to `main`, release-plz opens or updates a
`chore: release vX.Y.Z` PR that bumps the shared workspace version and adds the
`feat`, `fix`, `refactor` and `perf` commits to `CHANGELOG.md`. Merging it tags
`vX.Y.Z`. The tag runs `release.yml`, which builds the binaries and the install
script with [dist](https://opensource.axo.dev/cargo-dist/), creates the GitHub
Release, attaches `libdelldisplay`, and updates the formula in
`tatimblin/homebrew-tap`. Nothing is published to crates.io.

After changing `[workspace.metadata.dist]` in `Cargo.toml`, run `dist generate`
to regenerate `release.yml` rather than editing it by hand.

## Add a panel profile

Profiles live in `crates/ddc-panels/profiles/`, one TOML file per model or
family of models that share the same facts. At build time `build.rs` turns every
file there into Rust, and the CLI picks one by the EDID model string. Codes a
profile leaves out fall back to `crates/ddc-panels/mccs.toml`. The schema is in
[the profiles README](crates/ddc-panels/profiles/README.md).

1. Copy `dell-u4323qe.toml` (measured) or a close sibling (DDPM-derived).
2. Set `[panel] models` to the exact EDID model strings, e.g. `["DELL U2723QE"]`.
3. Fill `[quirks]` and one `[[code]]` per VCP code the panel advertises.
4. Give each code a `provenance`:
   - `observed`: you read or wrote it on this panel and saw the effect
   - `inferred`: from DDPM's code or a capture, not tried here
   - `spec`: MCCS says so, untested here
   - `unknown`: advertised, meaning not established
5. `cargo test -p ddc-panels`.

## Report a measured panel

Open an issue with:

- the model and firmware (`delldisplay identity firmware`)
- the capabilities string (`delldisplay caps`)
- `delldisplay status --json`
- what you wrote, what you read back, and what the screen did

Say which facts you saw on screen and which you only read back. The panel ACKs
writes it ignores, so a readback alone isn't proof.

Leave out your monitor's serial number and service tag. `identity edid` prints them;
redact before pasting.

## Don't commit

- Vendor binaries, app bundles, installers or dylibs
- Capture logs or disassembly of vendor software
- Anything under `/capture/` (it's gitignored for this reason)

Facts learned from those are fine to write down. The artifacts aren't.
