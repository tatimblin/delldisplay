# What DDPM believes about Dell's lineup

Read statically out of Dell Display and Peripheral Manager for macOS. Every claim
here is `inferred`: it is DDPM's model of its own hardware, not something tested
on these panels. Where it conflicts with the U4323QE profile, the profile wins.

Analysed build: DDPM 2.3.0.1005, installer `DDPMv2.3.0.1005.zip`, sha256
`ba5112785ed2f99ef8fd91e64a6afbc683c6140815f63aef2d4da979a79a1dc9`.

Apart from the U4323QE, the profiles in `crates/ddc-panels/profiles/` (16 files
covering 26 models) are compiled from this data and are unverified.

## How DDPM identifies a panel

- It keys on the macOS display name, not the service tag or capability string.
  The name is uppercased, `DELL` stripped, trimmed, and cut at `(` so
  `DELL U2724DE (HDMI)` becomes `U2724DE`. The service tag only keys per-display
  settings storage.
- A panel is driven when all three hold: the EDID vendor id contains `del`;
  `0xF1` bits 14/15 report DDC/CI enabled; and the name is in a compiled
  58-model list or the year digits in the name are 25 or higher.
- The year check takes the last two digits before the trailing letters:
  `U4323QE` gives 23, `S3225QC` gives 25. So any 2025+ Dell panel is supported
  without appearing in a table. DDPM also appends such panels to a persisted
  `Supported_List` at runtime; it ships empty.
- An unrecognised Dell panel isn't refused. It gets a "Limited SW mode" with some
  controls hidden.

## Per-model tables default permissive

The only per-model validation table (legal `0x60`/`0xE8` input codes) returns
"allowed" when a model has no entry. Generic windows for an unlisted panel:

- `0x60` low byte `0` or `0x01..0x1E`; high byte `0` or `0x01..0x85`
- `0xE8` three 5-bit fields, each `0` or `0x01..0x1E`
- `0xE9` `0x00..0x51`
- `0xE7`, `0xEE`, `0xF1` anything except `0xFFFF` (the read-failure sentinel)
- `0xEE` port codes `{0, 1, 8, 9, A, B, C, D}`

DDPM ships no capability database. Inputs, `0xE9` layouts and `0xE7` options all
come from the panel's own capability string at runtime. We do the same: gate on
Dell + DDC/CI enabled, read capabilities for everything per-panel, and treat the
tables below as narrowing hints, never as an allow list.

## Rosters

Compiled allow list (58, exact match after normalising):

    C2422HE   C2423H    C2722DE   C2723H    C3422WE   C5519Q    C5519QA   C5522QT
    C6522QT   C7520QT   C8621QT   P2424HEB  P2724DEB  P3424WEB  P5524Q    P5524QT
    P6524QT   P7524QT   P8624QT   U2421E    U2421HE   U2422H    U2422HE   U2422HX
    U2424H    U2424HE   U2520D    U2520DR   U2720Q    U2720QM   U2721DE   U2722D
    U2722DE   U2722DX   U2723QE   U2723QX   U2724D    U2724DE   U3023E    U3219Q
    U3223QE   U3223QZ   U3224KB   U3224KBA  U3419W    U3421WE   U3423WE   U3821DW
    U3824DW   U4021QW   U4320Q    U4323QE   U4919DW   U4919DWA  U4924DW   UP2720Q
    UP2720QA  UP3221Q

Reached only through the year check (from the bundled marketing-name list):

    P2425DE  P2425E   P2425HE  P2426E   P2426HE  P2426HEB P2426HEV P2725DE
    P2725HE  P2725QE  P2726DEB P2726DEV P2726HE  P3225DE  P3225QE  P3425WE
    P3426WEB P3426WEV P5525QC  P7525QT  S2725DC  S2725QC  S3225QC  S3425DW
    U2725QE  U3225QE  U3226Q   U5226KW

Network KVM (a separate host-to-host feature, not DDC): P2424HEB, P2724DEB,
P3424WEB, P5524Q, P5524QT, P6524QT, P7524QT, P8624QT, P5525QC, U3425WE, U4025QW.

## Per-model quirks

| Models | What differs |
|---|---|
| U4323QE | `0xE7` and `0xE5` writes go out twice (single-shot, then the retrying set). This is the double-write. No 1 s sleep after a `0xFF00` toggle; other models get one. |
| UP2720Q, UP2720QA, U4919DW, U4919DWA | After the `0xF010` swap, sleep 100 ms and write `0xE5 = 0xF000`. Other models write only `0xF010`. |
| U3223QE, U3023E, U2722D, U2722DE, U2722DX, U3219Q | `0xE9`, `0xE2` and `0xDC = 0` are written once with no retry. These panels blank when PxP turns off, so the reply is unreliable. |
| U2421E, U2520D | `0x10`, `0x12`, `0x62`, `0x8D` written once with no retry. |
| 27 Realtek-scaler models (below) | Every DDC read is preceded by a bus clean and a 100 ms sleep. |
| any panel | Realtek is detectable at runtime: `0xC8 & 0xFF == 0x09`. |
| U3224KB, U3224KBA | Uniformity compensation (`0xE4`) forced off even when advertised. HDR disabled on firmware `M2T104`. |
| U3224KB | `0xEE` port code 8 is driven as `0x0C` (Thunderbolt 1). Selecting input `0x1B` hides upstream code 8. |
| U3824DW | `0xEE` codes 0 and 1 mean USB-C1 / USB-C2, not USB-B. Codes 8-13 unused. |
| U4021QW | Port code `02` is labelled Thunderbolt. Label only. |
| U2724DE, U3224KB, U3419W, U3421WE, U3425WE | Whitelist of legal `0x60`/`0xE8` input codes (below). |
| UP3221Q, U4320Q, UP2720Q, U4323QE, UP2720QA | Asymmetric-PBP ("AA") flag, active when `0xE9` is in {0x24, 0x2F, 0x34, 0x36, 0x41, 0x51}. Behaviour not traced. |
| UP3221Q | Extra display-sync step. |
| UP\* except UP3218K/KA | Brightness is a fraction of the `0x10` max, floored at raw 45. DDPM's read path drops a `*100` in the below-floor branch. |
| UP32\*, UP27\* | `0xE2` uses calibrated preset codes (AdobeRGB, sRGB, BT.709, BT.2020, DCI-P3, USER/CUSTOM 1-3). |
| U2723QE, U3223QE, U3023E, U2723QX, U3223QZ | `0xE2` gains `0x3A` "Display HDR". |
| any panel whose `E2(...)` has `1E` | `0xE2` gains `0x04` GAME. |
| S3225QC, P3426WEB | PBP audio-source menu built from `F1`/`F2` tokens (Main/Sub). |
| 2025+ panels | `0xF2` bit 0 = brightness locked under HDR, bit 9 = contrast locked, bit 8 = iMST. |

Realtek-scaler models:

    U3219Q  U2520D  U2520DR U2421E  U2722D  U2722DE U2722DX U2422H
    U2422HE U2422HX C2722DE C2423H  U3223QE U3023E  C2723H  U2421HE
    U2721DE U3419W  U2424H  U2424HE U2724D  U2724DE U3425WE P2425HE
    P2425E  P2725HE U4025QW

Input whitelist (membership only; stored order matches capability-string order,
which may matter for `0xE7` field indexing, unconfirmed):

| Model | Legal `0x60` / `0xE8` codes |
|---|---|
| U2724DE | 0x19 TB, 0x0F DP1, 0x11 HDMI1 |
| U3224KB | 0x10 mDP1, 0x11 HDMI1, 0x19 TB |
| U3419W | 0x1B USB-C1, 0x0F DP1, 0x11 HDMI1, 0x12 HDMI2 |
| U3421WE | 0x0F DP1, 0x11 HDMI1, 0x12 HDMI2, 0x1B USB-C1 |
| U3425WE | 0x19 TB, 0x0F DP1, 0x11 HDMI1 |

## Transport habits DDPM applies to every model

- Bus clean before every write, then sleep 150 ms after `0xD6`/`0xE0`/`0xE1`,
  60 ms otherwise.
- Normal set: up to 4 attempts. Single-shot set: exactly one.
- On read result code 7, back off a random 1-3 s and retry.
- `0x8D`: write `0x0002` first, sleep 100 ms, then the real value.
- `0xE0` encoding depends on the capability string: absent = unsupported;
  `E0(...)` with more than one value = bitfield type; one value = boolean type.
- PxP apply: `0xE9` once, then `0x60` and `0xE8` each in a loop of up to 10
  attempts with 1 s sleeps. DDPM expects the panel to be unresponsive for about a
  second after a layout change.

## PiP / PBP

No per-model layout table. DDPM parses `E9(...)` from the panel and refuses values
not listed. The full layout vocabulary it knows:

| `0xE9` | Shape | Windows |
|---|---|---|
| 0x21 / 0x22 | PiP small / large | 1+1 |
| 0x23, 0x24 | side by side 50/50 (0x24 "fill") | 2 |
| 0x25-0x2E | side by side, asymmetric ratios | 2 |
| 0x2F | stacked 50/50 | 2 |
| 0x31 | 1 left + 2 right | 3 |
| 0x32 | 2 left + 1 right | 3 |
| 0x33 | 1 top + 2 bottom | 3 |
| 0x34 | 3 equal columns | 3 |
| 0x35 | 2 top + 1 bottom | 3 |
| 0x36 | 3 columns 25/50/25 | 3 |
| 0x41 | quad | 4 |
| 0x42 | 4 columns | 4 |
| 0x51 | stacked 33/67 | 2 |

DDPM's CLI slugs contradict its own icons and tooltips for the ratio layouts, so
go by shape, not slug. The U4323QE advertises 11 of these; pane counts for 0x32,
0x33 and 0x35 were confirmed on hardware and match DDPM.

- `0xE8`: `sub1 = v & 0x1F`, `sub2 = (v >> 5) & 0x1F`, `sub3 = (v >> 10) & 0x1F`,
  same on every model. Bit 15 is unused.
- `0xE5`: `0x0002` zoom, `0xF000 | a<<4 | b` swaps windows a and b (main 0,
  sub1 1, sub2 2, sub3 3). `0xF010` is the default swap sent to every model.
  `0xF000` alone is a no-op and suppressed, except as the follow-up frame above.

## USB / KVM

- USB-KVM is enabled when `E7` appears in the capability string. No model gate.
- Addressable binding (`0xE7 = 0xFF01..0xFF04`, bind window n) requires the literal
  token `FF` in the panel's `E7(...)` list. `FF` is a flag, not a port. The
  U4323QE advertises `E7(00 01 02 03)`, so DDPM's CLI won't offer it here. The GUI
  mouse-crossover path skips this check. DDPM filters `FF` case-insensitively but
  requires it case-sensitively; lowercase `ff` is refused.
- `0xFF00` is a blind toggle of the upstream.
- `0xEE`: four nibbles, nibble i = port code of upstream slot i. Codes: 0 USB-B1,
  1 USB-B2, 8 USB-C1, 9 USB-C2, A USB-C3, B USB-C4, C Thunderbolt 1,
  D Thunderbolt 2. DDPM's dictionary keys these as decimal strings (`"10"` is
  0xA).
- `0xE7` association map has two layouts, picked by a raw substring test for `EE`
  anywhere in the capability string:

  | | non-EE | EE |
  |---|---|---|
  | Width | 8-bit | 16-bit |
  | First input at | bits 0-1, climbing by 2 | bits 14-15, descending by 2 |
  | Field value | absolute port code | index into the `0xEE` nibble list |

  The U4323QE's string contains `EE`, so DDPM would pack it high-first. Our code
  ships low-first; the two agree only at the one input index tested on hardware.
  See [kvm.md](../kvm.md).

## Limits of this data

- It is a reading of the arm64 slice. KVC or selector-string access would not show
  up, and the x86_64 slice was not examined.
- DDPM is inconsistent with itself in places (ratio slugs, the `FF` case check,
  decimal vs hex `E7` token parsing, the UP brightness rescale). A belief read out
  of buggy code is a buggy belief.
- Absence from a table means DDPM doesn't check, not that a panel lacks the
  feature.
- The `0xEB` HID command table in Dell's SDK dylib is a separate namespace and
  nothing here comes from it.
- Open: what the asymmetric-PBP flag changes; which VCP carries the PiP inset
  position; whether `0xE2` is a preset index rather than a colour temperature on
  the U4323QE.
