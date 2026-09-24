# Picture and colour

Measured on a U4323QE; other panels differ, see their profile.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

Framing, checksums, the double-write and the reply shape are in
[PROTOCOL.md](PROTOCOL.md). Value names live in
`crates/ddc-panels/profiles/dell-u4323qe.toml` (or run `delldisplay codes`).

Provenance: spec (MCCS), observed (on this panel), inferred (from DDPM or a capture, not
exercised here), unknown.

## The codes

| Code | What it does | Provenance |
|---|---|---|
| `0x10` | Brightness, 0-100. Write round-trip verified. | observed |
| `0x12` | Contrast, 0-100. | observed |
| `0x14` | `color-preset`: the colour-temperature ladder plus Custom Color. Seven advertised values. | observed write; names inferred |
| `0xDC` | Picture mode. Advertises `00` (Standard) only. | observed |
| `0xE2` | `preset-mode`, a preset readback. Eight advertised values. DDPM only reads it. | observed read; meaning unknown |
| `0x16` `0x18` `0x1A` | Video gain red / green / blue. Read 100/100. Never written. | inferred |
| `0x04` | Restore factory defaults. Wipes every OSD setting, including input, PxP and KVM association. | spec |
| `0x05` | Restore brightness and contrast. | spec |
| `0x08` | Restore colour. | spec |
| `0x02` `0x52` | MCCS change notification (new-control-value flag, active-control FIFO). Inert here. | observed |

Everything else DDPM's colour pane touches is either host software or belongs to other
Dell panels (see the last two sections).

## Quirks

- `0x14` writes work. `0x06` was written and read back as `0x06`. The value names come
  from DDPM's table and nobody has checked them against the OSD, so a write lands on the
  right register and may carry the wrong label. Dell departs from MCCS here: `0x0B` is
  5700K (not "User 1") and `0x0C` is Custom Color (not "User 2").
- `0x14` reports `max = 0x0C`, the highest value, not a count. The legal values are
  sparse; take them from the capability string, never from `0..=max`.
- `0x14` reports its type byte as continuous. It's an enum. The type byte is not
  trustworthy on this panel; see [PROTOCOL.md](PROTOCOL.md).
- `0xDC` ignores writes it doesn't advertise. Writing `0x03` and `0x05` returned success
  and the register stayed `0x00`. The OSD's Movie and Game modes are not reachable over
  DDC. The general lesson holds for every code here: read back after writing.
- `0xE2` is not a mirror of `0x14`. In one DDPM poll the panel reported
  `0x14 = 0x05` (6500K) and `0xE2 = 0x00` (Standard) at the same time, and `0xE2` stayed
  `0x00` across an OSD preset change. It reports as non-continuous with max `0xFF`.
- `0x02`/`0x52` don't work as MCCS describes. After brightness was changed at the OSD
  (confirmed by reading `0x10`), `0x02` never moved off `0x02` and acknowledging with
  `0x01` changed nothing. `0x52` stayed at `0xE8` across five reads and never named
  `0x10`; it drifts between bursts and our own traffic seems to disturb it. DDPM's
  capture shows it running a drain loop against these registers, but here it tells you
  nothing. `delldisplay watch` polls and diffs instead.
- Brightness and contrast are stored per input, so they read differently after an input
  change.
- Clamp brightness to the `max` the panel reports rather than a hard-coded 100.

## What a colour UI should offer

Eight presets: Standard (via `0xDC`), 5000K, 5700K, 6500K, 7500K, 9300K, 10000K and
Custom Color (via `0x14`). Build the picker from the capability string, not from DDPM's
cross-model dictionary, which knows about forty.

## Not on this panel

Absent from the capability string. DDPM references them, so they turn up when reading
its code.

| Code | What DDPM uses it for |
|---|---|
| `0xF0` | Dell's gamut and HDR preset namespace (AdobeRGB, DCI-P3, Rec.709/2020, ComfortView, CAL1/2, game presets, HDR family). |
| `0x66` | Auto brightness and auto colour temperature. This panel has no ambient light sensor. |
| `0xE3` | Dark Stabilizer. Not to be confused with the DDC table-write command `0xE3`, which is a command opcode, not a VCP code. |
| `0x87` | MCCS sharpness. DDPM's sharpness slider is a webcam control. |
| `0x72` `0x90` | MCCS gamma and hue. DDPM never references them. |
| `0x86` | Aspect ratio. Read-only report field in DDPM's CLI. |
| `0xEA` `0xF4` | Written by DDPM's HDR toggle alongside `0x14`/`0xDC`/`0xF0`. Purpose unknown. |

Value-level gaps in codes that do exist: `0x14 = 0x01` (sRGB) and `0xDC = 0x02..0x06`
(Multimedia, Movie, Nature, Game, Sport) are not advertised.

## Open questions

- How `0xE2` relates to `0x14`. Write `0x14 = 0x0B` (5700K), read `0x14`, then `0xE2`.
  If `0xE2` becomes `0x0D` it tracks the active preset and the earlier `0x00` meant
  Standard mode; if it stays `0x00` it means something narrower.
- Whether `0xDC = 0x00` moves the panel to Standard from another preset. The only test
  wrote `0x00` while it already held `0x00`.
- Whether the `0x14` value names match the OSD. Step through all seven and read the OSD.
- Whether the gains do anything, and whether only under Custom Color. Select Custom Color,
  set red to 90, look for a cast; repeat under a fixed temperature.
- Whether `0xE2` is read-only in firmware. DDPM never writing it says nothing about the
  panel.
- Whether `0x05` and `0x08` are scoped as their names say. Set `0x10`, `0x12`, `0x14` to
  distinctive values, restore one scope, re-read all three.
- What `0x04` really resets. Destructive; `delldisplay` requires `--i-mean-it`.
- Whether `0x10` clamps or wraps above its max of 100.

## Host-side, no VCP

- Luminance slider in nits. The same `0x10`, converted host-side with a per-model max
  luminance.
- Smart HDR. The macOS HDR toggle; DDPM greys out brightness/contrast while it's on and
  rewrites the preset on toggle.
- ICC profile sync, both directions. Reads `0xE2`, sets the Mac's profile; or maps the
  profile name to a preset and writes `0x14`/`0xDC`.
- Per-app colour presets. Watches the frontmost app and writes `0x14`/`0xDC`.
- Brightness/contrast schedules and Matrix Control (fan-out to every monitor). Host
  timers writing `0x10`/`0x12`/`0x14`.
- PowerNap "reduce brightness on screensaver". The host lowers `0x10`. The panel's own
  `0xE0`/`0xE1` PowerNap booleans are covered in [system.md](system.md).
- Night Light (macOS Night Shift), Dynamic Contrast, Uniformity Compensation. No DDC
  writes.
- Response time, game modes, Vision Engine. Strings shared with gaming panels; nothing
  here.
- Sharpness and saturation sliders. Webcam controls.
