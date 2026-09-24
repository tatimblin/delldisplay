# delldisplay vs DDPM

What Dell Display and Peripheral Manager does over DDC on a U4323QE, and where
`delldisplay` stands on each. Other panels differ.

## Delivered

| Capability | VCP | Command |
|---|---|---|
| Brightness, contrast | `0x10`, `0x12` | `set brightness`, `set contrast` |
| Colour preset (writes work; value names unverified against the OSD) | `0x14` | `picture preset` |
| Preset readback (`preset-mode`) | `0xE2` | `status`, `get preset-mode` |
| Input select | `0x60` | `set input` |
| Which host owns the main window | `0x60` high byte | `kvm owner` |
| PiP/PBP layout, inset size | `0xE9` | `pxp` |
| PIP inset corner step (corner isn't readable, so it's tracked host-side) | `0xE9 = 0x02` | `pxp inset` |
| Sub-window sources 1-3 | `0xE8` | `pxp sub` |
| Whole PxP config in one go | `0xE9`, `0x60`, `0xE8` | `pxp apply` |
| Pane count and geometry | none | `pxp layouts`, `pxp geometry` |
| Swap two windows | `0xE5 = 0xF0XY` | `pxp swap` |
| USB upstream toggle | `0xE7 = 0xFF00` | `kvm switch` |
| USB association map, read and write (field order unresolved, see [kvm.md](kvm.md)) | `0xE7`, `0xEE` | `kvm map`, `kvm associate` |
| KVM wizard commit, in DDPM's write order | `0x60`, `0xE8`, `0xE9`, `0xE7` | `kvm commit` |
| Composite input / KVM state | several | `kvm status`, `pxp status` |
| Volume, and mute as DDPM does it (`0x62` = 0) | `0x62` | `picture volume`, `picture mute` |
| OSD Speaker switch (not MCCS mute on this unit) | `0x8D` | `picture status`, `set speaker` |
| OSD-lock and audio-enable status bits | `0x62` | `picture status` |
| OSD language | `0xCC` | `set osd-language` |
| Power, wake, PowerNap | `0xD6`, `0xE0`, `0xE1` | `picture power`, `wake`, `picture powernap` |
| Restore levels, colour, factory | `0x05`, `0x08`, `0x04` | `picture restore ... --i-mean-it` |
| Firmware version, scaler vendor | `0xC8`, `0xFD`, `0xC9` | `identity firmware` |
| EDID identity | I2C `0x50` | `identity edid` |
| Export / import settings | several | `identity export`, `identity import` |
| Busy gate before writes | `0xF2` bit 7 | built in |
| Notice changes made at the OSD | polling | `watch` |
| Raw get / set | any | `get`, `set` |

## Impossible on this panel

The panel advertises these and ACKs the write, then does nothing.

| Capability | VCP | What happens |
|---|---|---|
| Picture modes other than Standard | `0xDC` | Only `DC(00)` is advertised; other values ACK and stay `0x00`. |
| PBP zoom | `0xE5 = 0x0002` | ACKed, no visible change. |
| Underscan | `0xE5 = 0x0003` | ACKed, brief re-sync, no visible change. |
| Bind USB to a PxP window | `0xE7 = 0xFF01..04` | DDPM requires `FF` in `E7(...)`; this panel lists `E7(00 01 02 03)`. `kvm bind-window` refuses with exit 3. |
| MCCS change notification | `0x02`, `0x52` | Inert here. `watch` polls and diffs instead. |

## Declined

| Capability | VCP | Why |
|---|---|---|
| PBP audio source | `0x63` | Panel refuses (result `0x01`). |
| OSD lock / unlock | `0xCA` | Not advertised, and undoing a lock needs the physical joystick. |
| Blind repeated USB toggle | `0xE7 = 0xFF00` xN | Not idempotent; we check the host's USB tree instead. |
| USB-C prioritisation, audio profiles, spatial audio, mic controls, orientation, usage hours, ambient light, HDR and colour-space presets, dark stabiliser, asymmetric PBP ratios | various | Not advertised by this panel. |
| OSD settings clone | table opcodes | Panel's `cmds(...)` has no table opcodes. |

## Open

- Host roster: switching to a named machine. Every byte is reachable, there's just
  no name-to-input map yet.
- `0xF1` feature word: decoded, not surfaced, and bit 5 gates nothing here.
- `0xE7` field order: low-first ships, DDPM's high-first reading is untested.
- `0x14` value names: need checking against the OSD.

## Out of scope

Host-side features with no VCP behind them: Easy Arrange and window layouts,
hotkeys, menu-bar UI, per-app colour presets, brightness schedules, multi-monitor
sync, ICC management, Night Light, mouse-crossover USB switching, input renaming,
Dell Network KVM (a host-to-host network protocol), resolution and refresh
controls, firmware update over USB HID, and DDPM's own settings files.

Not in DDPM either: gamma, sharpness, dynamic contrast, response time, OSD timer
and transparency, power LED, USB charging, and turning DDC/CI itself on.
