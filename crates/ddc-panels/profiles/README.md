# Panel profiles

Each `*.toml` here describes one monitor, or a family of monitors that share the
same facts. `build.rs` compiles them all into the crate, and the CLI picks one by
matching the EDID model name against `models`.

A profile only lists what it knows about a panel. Any code it leaves out is looked
up in `../mccs.toml`, the standard MCCS 2.2 baseline.

## Schema

```toml
[panel]
models = ["DELL U2722D", "DELL U2722DE"]  # EDID model names, matched case-insensitively
mccs = "2.1"                              # optional, MCCS version the panel reports
default = true                            # optional, exactly one profile sets this

[quirks]                                  # optional, every key has a default
double_write = true                       # send every request frame twice
type_byte_reliable = false                # whether the reply's type byte can be trusted
refusal_result = 0x01                     # result byte the panel uses to decline a code
enumerated = [0x14, 0x60, 0xCC, 0xD6, 0xDC, 0xE2, 0xE9]  # low-byte enum codes
slow_write_codes = [0xE0, 0xE1, 0xD6]     # codes that need the longer settle delay
slow_write_ms = 150
normal_write_ms = 60

[[code]]
vcp = 0x60
name = "input"                            # lowercase a-z, 0-9, -
provenance = "observed"                   # see below
values = "0x0F=dp1, 0x11=hdmi1"           # optional, enum labels in the same charset
note = "One plain sentence, printed by `delldisplay codes`."
```

The defaults for `[quirks]` are the values shown above.

Provenance:

- `observed`: confirmed by reading or writing this panel
- `inferred`: taken from DDPM's code or a capture, not exercised here
- `spec`: MCCS standard, untested on this panel
- `unknown`: advertised by the panel, meaning not established

The build fails on unknown keys, unknown provenance, an empty `models`, a model
claimed by two profiles, names or value labels that aren't lowercase, and a name
that means different vcp codes in different profiles.

Not modelled yet: DDPM's single-shot (no retry) write codes and its Realtek
pre-read bus clean and 100 ms wait. The notes on the affected codes describe them.
