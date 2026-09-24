# delldisplay

Control Dell monitors over DDC/CI from the command line or from your own code:
brightness, inputs, PiP/PBP layouts, the built-in USB KVM and more. A pure-Rust
protocol core, a CLI (`delldisplay`), and a C ABI you can call from Swift, Go,
Python or anything else.

macOS only for now. Tested on a U4323QE. 16 more profiles covering 26 other Dell
models are compiled from DDPM data and unverified; anything else falls back to a
plain MCCS baseline.

## Quick start

1. Turn on DDC/CI on the monitor: OSD Menu › Others › DDC/CI.
2. Install it, one of:
   - Download `delldisplay-<version>-macos-universal.tar.gz` from
     [Releases](https://github.com/tatimblin/delldisplay/releases), extract, and
     put `delldisplay` on your `PATH`. The binary isn't signed, so if you
     downloaded it in a browser, clear the quarantine flag first:
     `xattr -dr com.apple.quarantine <extracted-dir>`.
   - With Rust 1.87+: `cargo install --git https://github.com/tatimblin/delldisplay --locked ddc-cli`
     (the package is `ddc-cli`; the command it installs is `delldisplay`).
   - From a clone: `cargo install --path crates/ddc-cli`.
3. Try it:

```sh
delldisplay status
delldisplay set input usb-c
```

## CLI

Every command takes `--json` and `--display <N>`. Run `delldisplay --help`, or
`--help` after any command, for the full list.

- Reading: `list`, `status`, `get`, `caps`, `codes`, `coverage`
- Picture preset, volume, mute, power, PowerNap, restores: `picture`
- PiP/PBP layouts, sub-window sources, swaps: `pxp`
- USB KVM (association map, commit, switch): `kvm`
- EDID, firmware, settings export/import: `identity`
- Watch for changes made with the monitor's own buttons: `watch`
- Wake from standby: `wake`
- Step through a code's values to see what each does: `map`, `pipmap`

```sh
delldisplay set brightness 60
delldisplay set 0x10 60              # same thing, raw code
delldisplay pxp apply quad --main usb-c --sub1 dp
delldisplay kvm ensure here --dry-run
```

Every writing verb takes `--dry-run` (read and check, write nothing) and
`--settle-ms <MS>` (cap each settle wait). `map` and `pipmap` are the
exception: they write the panel over and over on purpose, so stay at the screen.
Their `--values` list is decimal unless written as `0x..`.

Exit codes: `0` ok, `1` the panel or transport failed, `2` usage, `3` refused
(nothing written).

### JSON output

With `--json` every command prints one JSON object on stdout, errors included
(`{"ok": false, "error": "..."}`). Usage errors (exit 2) stay plain text on
stderr. Reads print their own shape. Every write prints the same report:

```json
{ "ok": true, "action": "swap main<->sub1", "dry_run": false,
  "refused": [], "warnings": [], "steps_run": 1, "failure": null,
  "world_changed": true, "wrote": [{"code": 229, "hex": "0xE5", "value": 61456}],
  "reads": [], "undo": [], "undo_complete": true, "undo_gaps": [], "notes": [] }
```

- `ok`: the plan ran to the end. False when refused or failed.
- `refused`: why a guard said no. Non-empty means nothing was written (exit 3).
- `warnings`: worth knowing; the write went ahead anyway.
- `failure` and `steps_run`: the step that stopped the plan, and how far it got.
- `world_changed`: a write went out. DDC writes aren't acknowledged, so a failed
  one may still have landed.
- `wrote`: the writes sent. Called `would_write` under `--dry-run`.
- `reads`: values read back during the plan.
- `undo`, `undo_complete`, `undo_gaps`: writes that put the panel back after a
  failure, and any codes that couldn't be saved first.
- `notes`: anything else the command has to say.

Some commands add fields (`identity import` adds `monitor_match` and `skipped`).

## Safety

- KVM and association writes (`0xE7`) move the monitor's USB hub between hosts.
  If your keyboard and mouse hang off that hub, a bad write can strand them on
  another machine. Keep a Bluetooth or directly-attached keyboard handy.
- Factory-restore verbs need `--i-mean-it` and can't be undone.
- The panel ACKs writes it ignores, so an ok from a write proves nothing. The
  CLI reads back where it can.
- Malformed vendor HID reports once wedged the monitor's USB hub. Replugging the
  upstream cable or power-cycling the monitor brought it back. `delldisplay`
  doesn't use that path.

## Every request is written twice

Dell's own software sends every request frame twice before reading, and on the
U4323QE a single write gets the DDC Null Message nearly every time. It's a
transport policy, on by default: `Policy::double_write`, or `--no-double-write`
on the CLI. Details in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Crates

    ddc-core/       platform-independent protocol, no OS calls, no unsafe
                    frames and checksums, capability parser, VCP registry,
                    PxP/KVM/picture logic, write guards and plans
    ddc-transport/  the I2c trait and the DDC session (retries, timing,
                    double-write); macos.rs is the IOAVService backend
    ddc-panels/     per-panel profiles as TOML in profiles/, turned into
                    Rust at build time and selected by EDID model
    ddc-cli/        the `delldisplay` binary
    ddc-ffi/        cdylib/staticlib plus include/delldisplay.h

Porting to another OS means implementing the `I2c` trait:

```rust
pub trait I2c {
    fn write(&mut self, chip: u8, offset: u8, data: &[u8]) -> Result<(), Error>;
    fn read(&mut self, chip: u8, offset: u8, out: &mut [u8]) -> Result<(), Error>;
}
```

## Rust

```rust
use ddc_transport::macos;
use ddc_core::vcp::{Vcp, input};

let mut d = macos::open_first()?;
println!("{}", d.get(Vcp::BRIGHTNESS)?.current);
d.set(Vcp::INPUT_SOURCE, input::USB_C as u16)?;
let caps = d.capabilities()?;
println!("{:?}", caps.legal_values(Vcp::PIP_MODE));
```

## C / Swift

```c
#include "delldisplay.h"
DdHandle *h = dd_open(0);
uint16_t cur, max;
dd_get(h, DD_VCP_BRIGHTNESS, &cur, &max);
dd_set(h, DD_VCP_INPUT, DD_INPUT_USBC);
dd_close(h);
```

Build with `cargo build --release` and link `target/release/libdelldisplay.dylib`
(or the `.a`), or grab `libdelldisplay-<version>-macos-universal.tar.gz` from
Releases: it has the universal dylib, the static lib and `delldisplay.h`.

## Docs

- [docs/PROTOCOL.md](docs/PROTOCOL.md): wire format and the rules this panel
  plays by
- Features: [input](docs/input.md), [pxp](docs/pxp.md), [kvm](docs/kvm.md),
  [picture](docs/picture.md), [audio and OSD](docs/audio-osd.md),
  [system](docs/system.md)
- [docs/vs-ddpm.md](docs/vs-ddpm.md): what we cover compared with Dell's app
- [docs/research/ddpm-models.md](docs/research/ddpm-models.md): what Dell's app
  believes about other models
- [tools/](tools/README.md): research tools
- [CONTRIBUTING.md](CONTRIBUTING.md)

## License

MIT. See [LICENSE](LICENSE).

Not affiliated with or endorsed by Dell. Built by observing DDPM for
interoperability.
