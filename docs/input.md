# Input and source selection

Measured on a U4323QE; other panels differ, see their profile.

Which source feeds which window: the main input (0x60), the PiP/PBP sub-window
sources (0xE8), and the feature bitmask DDPM checks before showing its Input
Source pane (0xF1). Layouts (0xE9) and window actions (0xE5) are in
[pxp.md](pxp.md). USB upstream association (0xE7, 0xEE) is in [kvm.md](kvm.md).

Framing, checksums, the double-write and the reply shape are in
[PROTOCOL.md](PROTOCOL.md). Legal values and labels for every code are in
`crates/ddc-panels/profiles/dell-u4323qe.toml`, or run `delldisplay codes`.

Provenance: spec (MCCS), observed (read or written on this panel), inferred
(from DDPM's code or a capture, not exercised here), unknown.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

## 0x60: main input, low byte

The input the main window shows. Write the input code in the low byte, high
byte zero. The panel advertises `60(1B 0F 13 11 12)`: usb-c, dp1, dp2, hdmi1,
hdmi2. Observed.

Watch the codes: 0x13 is dp2 and 0x12 is hdmi2. Restoring 0x12 thinking it was
dp2 puts the panel on a dead input.

Quirks, all observed:

- The type byte says continuous. 0x60 is an enumeration; the profile's
  `enumerated` list overrides the wire.
- `max` reads 0x000E, which is neither the input count nor the largest code.
  Check writes against the `60(...)` list, never against `max`.
- While Auto Select hunts empty inputs, the panel walks its input list and a
  single read returns wherever it was parked. `Ddc::sample_mode` takes several
  reads and returns the most common one. Turning Auto Select off in the OSD
  makes every read trustworthy.
- A 0x60 write that collides with an input already assigned to a sub-window is
  accepted (about 170 ms) and leaves 0xE8 alone. DDPM's write order (0x60, then
  0xE8) is safe here.

DDPM labels inputs from the capability list alone: if `12` is present, 0x11
becomes "HDMI1"; if `13` is present, 0x0F becomes "DP1". No DDC traffic is
involved. Inferred.

DDPM also carries a small per-model input whitelist. The U4323QE is not in it,
so the capability list is the only real constraint. Inferred.

## 0x60: high byte is the arrival port

The high byte is not a copy of the low byte. It names the port the DDC request
arrived on. Observed on both transports:

- over DP-AUX it holds the input code of the port this host is plugged into;
- over the USB-HID tunnel it holds a pseudo-port in 0x80-0x85.

Same panel, same session, same low byte: `13 13` over AUX, `83 13` over HID.
`HH != LL` is a real state, seen as `13 0F`: this host is on dp2 while another
machine holds the main window on dp1.

```
LL = value & 0xFF     what the main window shows
HH = value >> 8       which port asked
HH == LL              this host owns the main window
HH in 0x80..0x85      the request came over USB, not video
```

`ddc_core::kvm::main_owner` decodes this; `delldisplay kvm owner` prints it.

DDPM validates every 0x60 read before caching it: `HH` must be 0 or 1..0x85,
`LL` must be 0 or 1..0x1E. A read that fails is discarded. `bits::InputWord::is_valid`
is the same predicate.

Which pseudo-port (0x80-0x85) maps to which USB upstream is inferred from DDPM:
0x80/0x81 for USB-B1/B2, 0x82-0x85 for USB-C1..C4. We saw 0x82 and 0x83 in two
cabling states, which fits, but nothing has pinned which upstream gives which
digit. Don't build logic on it.

## 0xE8: sub-window sources

Three packed 5-bit fields, each a 0x60 input code. The main window is not in
here; it stays on 0x60. Observed.

```
bit 15 | 14..10 | 9..5 | 4..0
unused |  sub3  | sub2 | sub1
```

`E8 = sub1 | sub2 << 5 | sub3 << 10`. Zero means unassigned. Confirmed by
predicting 0x6DEF and 0x6DFB and having the panel accept them.

- `max` reads 0xFFFF. Neither the reply nor the capability list says which
  writes the panel takes.
- Always read-modify-write. Changing sub1 must keep sub2 and sub3.
- A sub2/sub3 write only makes sense when the active 0xE9 layout has that many
  windows; see [pxp.md](pxp.md) for pane counts.
- Main and a sub-window may hold the same source. With main on dp2 and quad lit,
  writing 0xE8 = 0x4DF1 put dp2 in sub3 too and the panel showed this host in
  both cells. DDPM's "a source cannot be both main and sub" rule is its own
  convention, not a panel precondition. `delldisplay` warns instead of refusing.

## 0xF1: feature bitmask (read-only)

DDPM reads it first, before anything else. This panel reads 0xC12B, and reports
that as its own `max` too. Bits 15, 14, 8, 5, 3, 1, 0 are set.

- Bit 14: DDPM's "supports DDPM" allow-list flag. DDPM drops the display if it
  is clear. Inferred.
- Bit 5: DDPM shows its PBP-zoom and underscan hotkey rows when set. It does not
  gate the actions: the bit is set here and both actions are inert (see
  [pxp.md](pxp.md)). Don't gate anything on it.
- Bits 15, 8, 3, 1, 0: unknown.

Nothing in DDPM writes 0xF1. Don't.

## DDPM's Input Source pane

The pane reads 0xE9, 0x60, 0xEE, 0xE7, 0xE8 in that order, and refuses to
render unless all of 0x60, 0xE7, 0xE8, 0xE9, 0xEE, 0xF1 pass its validators.
Reading 0xE9 first is worth copying: you know how many windows exist before you
interpret 0xE8. Inferred.

## Not on this panel

- 0x63, PiP/PBP audio source. Absent from the capability string; a read comes
  back with result 0x01. Observed.
- 0xEA, USB-C prioritization (0xF800 high resolution, 0xF801 high data speed in
  DDPM). Not advertised. The OSD has the setting; DDC doesn't. Untested.
- 0xE2 is not a sub-input. It's `preset-mode`, a read-only preset report; see
  [picture.md](picture.md).

## Host-side, no VCP

- Auto Select, Auto Select for USB-C, Reset Input Source. No string, no code,
  nothing in DDPM's CLI. Their only visible effect is the 0x60 read churn above.
- Rename Inputs. Nicknames live in DDPM's settings JSON.
- Input hotkeys (toggle two inputs, favourite input, video swap, PiP position,
  PBP zoom, underscan). Stored host-side; the effect is a plain write to 0x60,
  0xE8, 0xE5 or 0xE9.
- "Source cannot be both main and sub." DDPM checks it locally; the panel
  doesn't care.
- Easy Arrange, Network KVM. Host software; see [kvm.md](kvm.md).

## Open questions

- Is `HH` the arrival port on every read, or the last port that changed? With
  two hosts, set main to the other host's input and read 0x60 from this one.
  Expect `HH` = this host's port.
- Which USB upstream produces which pseudo-port (0x80-0x85)? Read 0x60 over the
  HID tunnel from a USB-C upstream, then a USB-B one, and compare.
- Is 0xEA refused by the panel? Read it once (`82 01 EA 56`) and record the
  result byte.
- 0xEF and 0xFE are advertised and DDPM never touches them. A blind read
  (`82 01 EF 53`, `82 01 FE 42`) would at least record the reply shape.
- 0xF1 bits 15, 8, 3, 1, 0. Needs another Dell model's value to diff against.
- Whether `HH` is populated on other Dell models. Only this panel is measured.
