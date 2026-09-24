# Power, identity and diagnostics

Measured on a U4323QE; other panels differ, see their profile.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

Covers power (`0xD6`, `0xE0`, `0xE1`), the read-only identity codes, the status
words (`0xF1`, `0xF2`), change notification (`0x02`, `0x52`), the settings-clone
table (`0xEF`) and the unidentified `0xFE`. Framing, checksums, the double-write
and the refusal byte are in [PROTOCOL.md](PROTOCOL.md). Restores
(`0x04`/`0x05`/`0x08`) are in [audio-osd.md](audio-osd.md). Value names are in
`crates/ddc-panels/profiles/dell-u4323qe.toml`; `delldisplay codes` prints them.

Provenance: spec (MCCS, untested here), observed (read or written on this
panel), inferred (from DDPM's code or a capture), unknown.

Model, serial, service tag and manufacture date come from EDID (I2C `0x50`), not
from any VCP. `delldisplay identity edid` reads them; `identity firmware` reads
the firmware codes below.

## Summary

| Code | What | Provenance |
|---|---|---|
| `0xD6` | power mode: `01` on, `04` standby, `05` off | observed |
| `0xE0` | PowerNap dim, boolean | observed (read) |
| `0xE1` | PowerNap sleep, boolean | observed (read) |
| `0xC8` | controller id, low byte = scaler vendor; reads `0x05` (MStar/MediaTek) | observed |
| `0xC9` | firmware level; reads `0x0001` | observed |
| `0xFD` | firmware letter; reads `0x74` (`t`) | observed |
| `0xB6` | display technology; reads `0x03` (LCD active matrix) | observed |
| `0xB2` | sub-pixel layout; reads `0x01` (RGB vertical stripe) | observed |
| `0xC6` | application enable key; reads `0xCC` | observed |
| `0xDF` | MCCS version | observed |
| `0xAC` `0xAE` | h/v frequency; encoding doesn't match MCCS | unknown |
| `0xF2` | status word; bit 7 = busy | observed (idle read) |
| `0xF1` | Dell feature/allow-list word; reads `0xC12B` | observed (value), inferred (bits) |
| `0x02` `0x52` | new-control-value / active-control | unknown (inert here) |
| `0xEF` | settings-clone table | inferred |
| `0xFE` | advertised, meaning not known | unknown |

## Power, 0xD6

Legal values per the capability string are `01`, `04`, `05`. MCCS calls `04`
"off (reduced power)"; DDPM labels it "Standby". We use `on` / `standby` / `off`.

`0xD6`, `0xE0` and `0xE1` need a longer settle: DDPM sleeps 150 ms before these
writes and 60 ms before others. The transport sleeps 150 ms both before and after
for these three, which can't be wrong. The list is `slow_write_codes` in the
profile.

On Apple Silicon DDPM's first read on a new handle is `0xD6`, retried up to 5
times. It's the cheapest liveness probe.

## Wake

A bare `0xD6 = 1` isn't a wake. A panel that went down through PowerNap reads
"on" and goes back to sleep, because the PowerNap flags are still set. DDPM's
block, retried until `0xD6` reads `1`:

1. `0xD6 = 1`
2. `0xE0 = 0`
3. `0xE1 = 0`
4. read `0xD6`

`delldisplay wake` does this, up to 5 tries.

## PowerNap, 0xE0 / 0xE1

DDPM picks the encoding from the capability string, not a read (inferred):

- no `E0`: unsupported
- `E0(...)` with a value list: `0xE0` is a bitfield, bit 0 dim, bit 1 sleep,
  cleared with `& 0xFC`
- bare `E0`: `0xE0` and `0xE1` are two booleans

This panel advertises bare `E0` and `E1`, so it's two booleans. Both read `0`,
max `1`. `delldisplay picture powernap` branches on the encoding.

PowerNap is host-orchestrated. The monitor doesn't know what a screensaver is;
DDPM watches macOS and then writes these registers.

## Identity codes

All read-only.

DDPM builds a firmware string from `0xC8`, `0xFD`, `0xC9` read in that order with
100 ms gaps, aborting if any returns `0xFFFF`. It keeps `0xC8` whole, prints
`0xFD` as an uppercase character and `0xC9 >> 8` as hex. The exact concatenation
and separator aren't known. Compare against the OSD (Others › Firmware) before
hard-coding a format.

`0xC8`'s low byte decodes as `05` MStar/MediaTek, `09` Realtek, `12` Novatek.

`0xC6` is an MCCS token an app reads to confirm which monitor it's talking to.
DDPM never reads it. Whether it changes across a power cycle is untested.

DDPM never reads `0xDF`; it takes `mccs_ver(2.1)` from the capability string. So
does `delldisplay`.

## Frequencies, 0xAC / 0xAE

`0xAC` reads current `0x00B4`, max `0x0002`. `0xAE` reads current `0x006F`, max
`0x0000`. Neither fits MCCS units at a 60 Hz mode.

One guess for `0xAC`: `(max << 16) | current` = 131,252, which is 131.25 kHz and
plausible for 3840x2160 @ 60 Hz. It fits one reading. `0xAE` has no candidate.
Don't expose either as a refresh rate.

## Status word, 0xF2

Full 16-bit value; don't mask to the low byte. Reads `0x0000` when idle. Bit
meanings are inferred from DDPM:

- bit 0: HDR suppresses brightness
- bit 6: HDR active
- bit 7: panel busy (OSD open, mid-operation)
- bit 8: DP MST on
- bit 9: HDR suppresses contrast

Gate writes on bit 7. `delldisplay` does. Bits 0 and 9 explain a brightness or
contrast write that gets ignored while HDR is on.

## Feature word, 0xF1

Reads `0xC12B` here, seven bits set.

- bit 14: DDPM's allow-list / liveness flag. Set here. If it's clear DDPM treats
  the display as unsupported. A failed read (`0xFFFF` or no reply) means DDC/CI
  is off, not that the monitor isn't a Dell.
- bit 5: DDPM shows the PBP zoom and underscan controls when it's set. It's set
  here and both actions are inert (see [pxp.md](pxp.md)), so it doesn't gate
  them on this panel.
- bit 15: named next to bit 14 in a DDPM log string; no separate test found.
- the other four set bits are unexplained.

Don't gate features on `0xF1` beyond bit 14.

## Change notification, 0x02 / 0x52

MCCS says `0x02` reads `0x02` after a control is changed at the OSD, `0x52` names
the changed code and self-clears, and writing `0x02 = 1` acknowledges.

It doesn't work that way here. With brightness changed at the OSD (and confirmed
by reading `0x10`), `0x02` stayed at `0x02` before and after, and the ack changed
nothing. `0x52` sat at `0xE8` across reads and never named `0x10`; it drifts
between bursts and our own traffic seems to disturb it.

`delldisplay watch` polls a set of codes and diffs them instead. For the record,
DDPM precedes its ack with three undoubled `82 01 00 BC` frames and no reply
read.

## Settings clone, 0xEF

Dell's Monitor Settings Management (OSD clone) reads `0xEF` with the MCCS table
opcodes, not get/set (inferred). Read request: `84 E2 EF p1 p2 ck`, then wait
300 ms and read 38 bytes. DDPM reads pages `(20,00)` and `(21,06/16/26/36/46)`,
and writes with opcode `0xE7` at pages `(0A,06..46)`.

This panel may not do table transactions at all: `cmds(01 02 03 07 0C E3 F3)`
doesn't list `E2`, `E4` or `E7`. Dell ships a "this model does not support OSD
monitor cloning" string for that case. Not implemented.

## 0xFE

Advertised. DDPM never reads or writes it. Don't blind-write; it sits near the
firmware area. Not to be confused with the HID tunnel's `0xFE` refusal status or
the SDK's command id `0xFE`, which are different namespaces.

## Codes this panel doesn't advertise

DDPM reads these on other monitors: `0xC0` (usage hours), `0xAA` (orientation),
`0x0E` (input clock), `0xCA` (OSD lock). None are in this panel's capability
string. Expect result `0x01` or a Null Message.

## Host-side, no VCP

- EDID identity: model, serial, service tag, manufacture date, size.
- DDC/CI on/off: an OSD item (Menu › Others › DDC/CI). DDPM can only detect it's
  off, by `0xF1` failing.
- Diagnostic and asset reports: host file writers built from EDID, the
  capability string and a few VCP reads.
- Firmware update: USB, from a downloaded package.
- PowerNap scheduling: macOS screensaver notification plus a DDPM preference.
- DDPM's "Advanced Control": a raw get/set, same as `delldisplay get` / `set`.

## Open questions

1. Does `0xD6 = 04` differ from `05` visibly (amber LED vs dark)?
2. Is the 150 ms settle needed at all? Two `0xE0` writes under 150 ms apart would
   tell.
3. The firmware string's concatenation order, against the OSD.
4. `0xAC` / `0xAE` encoding: read both at 60 Hz and 30 Hz and look for a ratio.
5. `0xF2` bits: read with the OSD open vs closed (bit 7), HDR on/off (0, 6, 9),
   MST on (8).
6. Does `0xEF` answer a table read? Expect a Null Message given `cmds(...)`.
7. What is `0xFE`? One GET, record type/max/current.
8. `0xC6` stability across a power cycle.
