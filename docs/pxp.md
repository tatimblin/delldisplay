# PiP / PBP (PxP)

Measured on a U4323QE; other panels differ, see their profile.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

Framing, checksums and the double-write are in [PROTOCOL.md](PROTOCOL.md). Value names
live in `crates/ddc-panels/profiles/dell-u4323qe.toml` (or run `delldisplay codes`).

Provenance: spec (MCCS), observed (on this panel), inferred (from DDPM or a capture, not
exercised here), unknown.

## The codes

| Code | What it does | Provenance |
|---|---|---|
| `0xE9` | Layout. `0x00` is off. One write picks the whole arrangement. | observed |
| `0xE8` | Sub-window sources: three packed 5-bit fields, `sub1 \| sub2<<5 \| sub3<<10`, each a `0x60` input code. Bit 15 unused. | observed |
| `0x60` | Main window source. Same code space as the `0xE8` fields. | observed |
| `0xE5` | Window actions: swap (`0xF0XY`), PBP zoom (`0x0002`), underscan (`0x0003`). Write-only. | observed (swap), observed inert (zoom, underscan) |

The capability string lists `E9(00 01 02 21 22 24 2F 31 32 33 34 35 41)`, and bare `E5`
and `E8` with no value lists. PiP position and size are values of `0xE9`, not separate
codes. `0x63` (PiP/PBP audio source) is not advertised and a GET returns result `0x01`.

## Layouts (`0xE9`)

Thirteen advertised values, eleven distinct layouts. "Main" is the window whose source
is `0x60`; in the CLI names "self" means the host sending the command, assumed to be on
main.

Sub windows are numbered clockwise from main, not in reading order. The rule was derived
from `0x32` and `0x35`, then predicted `0x33` and `0x41` before they were looked at, and
matches `0x31`.

| E9 | Shape | main | sub1 | sub2 | sub3 |
|---|---|---|---|---|---|
| `00` | off, fullscreen | full | - | - | - |
| `21` | PiP, small inset | full | inset | - | - |
| `22` | PiP, large inset | full | inset | - | - |
| `24` | 2 side by side, 50/50 | left | right | - | - |
| `2F` | 2 stacked, 50/50 | top | bottom | - | - |
| `31` | L1R2 | left, full height | top-right | bottom-right | - |
| `32` | L2R1 | right, full height | bottom-left | top-left | - |
| `33` | T1B2 | top, full width | bottom-right | bottom-left | - |
| `34` | 3 columns | left | middle | right | - |
| `35` | T2B1 | bottom, full width | top-left | top-right | - |
| `41` | 2x2 quad | top-left | top-right | bottom-right | bottom-left |

All observed. An unfed pane renders black and looks absent, so count panes with every
cell fed. With every cell fed, `0x32`, `0x33` and `0x35` each show three panes,
matching DDPM's table.

DDPM's table also has layouts this panel does not advertise: asymmetric splits
(`0x23`, `0x25`-`0x2E`, `0x51`), `0x36` (25/50/25 columns) and `0x42` (four columns).
Don't offer a split-ratio control here. Whether the panel refuses or ignores them is
open.

### `0x01` and `0x02` depend on the current layout

- From PiP (`0x21`/`0x22`): `0x02` steps the inset to the next corner, `0x01` steps the
  size (`0x21` -> `0x22`). The layout reads back unchanged.
- From off (`0x00`): they act as aliases. `0x01` reads back `0x32`, `0x02` reads back
  `0x24`.

So fold aliases only when the prior value was `0x00`. Prefer writing `0x21`/`0x22`
directly for size; DDPM never writes `0x01`.

The inset corner is not readable over DDC. DDPM keeps a host-side mod-4 counter and
writes a blind `0x02`; any reimplementation drifts as soon as someone moves the inset
from the OSD.

### Writing a layout

- A layout write returns in about 158 ms. Single reads usually keep working right
  after, but the panel can go quiet for a second while it re-syncs; a capabilities
  read in that window got no reply, so that read retries for about 3 s.
- Some layouts change `0x60` on their own. Snapshot `0x60` before and restore it after.
- An input change (not a layout change) makes the panel re-sync; the transport
  reconnects.
- Dell stores picture settings per input, so brightness reads differently after an
  input move.
- The reply reports `0xE9` as continuous with max `0xFF`. Validate writes against the
  capability string, not the reply.

## Sub-window sources (`0xE8`)

```
E8 = sub1 | (sub2 << 5) | (sub3 << 10)
     bits 0-4  bits 5-9    bits 10-14
```

- Read-modify-write. Read `0xE8`, replace one field, write the whole word. DDPM does the
  same. Writing a bare input code clobbers the other two fields.
- The packing was confirmed by prediction: `0x6DEF` (sub1 = dp1) and `0x6DFB` were
  computed first and the panel accepted both.
- `0x6DF3 & 0x1F = 0x13`, which is dp2, not dp1.
- Fields for windows the current layout doesn't have keep stale values. Don't assume a
  reading describes only live windows.
- Main and a sub window may hold the same source. With main on dp2 in quad, writing
  `0xE8 = 0x4DF1` put dp2 in sub3 too and the panel showed the host in both cells. The
  reverse (writing a colliding `0x60`) is accepted as well. "A source can't be both main
  and sub" is DDPM's convention; `delldisplay` warns instead of refusing.
- `0xE8` reads as continuous, max `0xFFFF`. No help validating a write.

### Write order

DDPM writes layout, then main input, then sub sources: `0xE9` -> `0x60` -> `0xE8`, with
sleeps between. That order is safe here: writing `0x60` while `0xE8` already holds the
same source in quad was accepted in 173 ms and left `0xE8` alone. Whether the order is
required is open.

## Window actions (`0xE5`)

One register, unrelated value spaces. Keep them apart in code.

Swap: `0xF000 | X << 4 | Y`, where X and Y are window indices (0 main, 1 sub1, 2 sub2,
3 sub3), the same indices as the `0xE8` fields. `0xF010` and `0xF001` both mean "swap
0,1"; `0xF010` is the form DDPM sends and the only one tested.

- Swap needs the double-write like every other request. A single `84 03 E5 F0 10 BD`
  does nothing; the doubled frame swaps. On the U4323QE DDPM sends `0xF010` twice (a
  single-shot write, then its retrying Set), which is the double-write.
- DDPM follows `0xF010` with a `0xF000` release about 100 ms later only for the
  UP2720Q/QA and U4919DW/DWA. The doubled bare `0xF010` swaps on this panel.
  `delldisplay` offers the release as an opt-in.
- DDPM only offers the quick swap in two-window layouts (`0xE9` non-zero and `<= 0x2F`,
  or `0x51`).
- `0xE5` reads `0x0001` before and after every write. Reading it says nothing about what
  happened.

PBP zoom (`0x0002`) and underscan (`0x0003`) are ACKed and do nothing visible here, single
or repeated, in a 2-up layout. Underscan re-syncs the panel for about a second of black
and leaves no border. DDPM shows these controls when `0xF1` bit 5 is set; it is set on
this panel, so bit 5 is not a gate here.

## Max windows

Pure capability arithmetic, no traffic. DDPM's rule: `41` or `42` in the `E9` list means
4 windows; any of `31`-`36` means 3; any of `23`-`2F` or `51` means 2; otherwise 1. PiP
is supported if `21` or `22` is listed, PiP position if `02` is listed. For this panel:
4 windows, PiP and PiP position supported.

## Open questions

- Does `0xF0XY` address windows independently? In quad with four sources, `0xF023`
  should swap only sub2 and sub3.
- Is `0xF000` alone a no-op from a settled state, or a swap of window 0 with itself?
- Are unadvertised layouts (for example `0x25`) refused, ignored, or live?
- Is `0xE9` -> `0x60` -> `0xE8` required, or just DDPM's habit? Try `0xE8` before `0xE9`
  when entering a three-window layout.
- Do the OSD's PIP/PBP submenu items map to these codes the way DDPM's control names
  suggest (Mode = `0xE9`, Sub = `0xE8`, Video Swap = `0xE5`, Position/Size = `0xE9`
  `0x02`/`0x01`)?

## Host-side, no VCP

- PxP hotkeys (video swap, PBP zoom, PiP positioning). App-side key capture; each fires
  one of the writes above.
- PiP corner tracking. A host counter, see above.
- Mouse crossing between PBP windows and auto-switching USB in PBP. Host cursor
  tracking; the only panel write is the `0xE7` USB toggle ([kvm.md](kvm.md)).
- Easy Arrange, custom layouts, "reposition screens". macOS window tiling. Unrelated to
  `0xE9` despite the shared word "layout".
- The HID tunnel's PxP commands (`GetPxPLayout` and friends) belong to the USB hub's
  command set, not VCP. See [PROTOCOL.md](PROTOCOL.md).
