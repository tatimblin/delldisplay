# Changelog

## 0.1.0 (2026-09-23)

First public release.

- `ddc-core`: DDC/CI framing and checksums, capabilities parser, VCP registry,
  EDID parser, PxP layouts and geometry, USB-KVM association map, picture and
  power logic, write guards and plans with undo.
- `ddc-transport`: `I2c` trait, DDC session with retries and double-write, a
  plan runner with `--dry-run` support, macOS `IOAVService` backend.
- `ddc-panels`: panel profiles as TOML, compiled at build time and selected by
  EDID model. 17 profiles cover 27 models, with an MCCS baseline for anything a
  profile leaves out. The U4323QE is measured; the rest come from DDPM data and
  are unverified.
- `ddc-cli`: the `delldisplay` binary. Reads with `list`, `status`, `get`,
  `caps`, `codes`, `coverage`, `identity` and `watch`; writes with `set`,
  `picture`, `wake`, `pxp` and `kvm`; `map` and `pipmap` for exploring. `--json`
  everywhere, one report shape for every write, and exit codes that separate
  failures from refusals.
- `ddc-ffi`: C ABI and `delldisplay.h`. Not published to crates.io.
