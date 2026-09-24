# Audio and OSD

Measured on a U4323QE; other panels differ, see their profile.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

Framing, checksums, the double-write and the reply shape are in
[PROTOCOL.md](PROTOCOL.md). Value names live in
`crates/ddc-panels/profiles/dell-u4323qe.toml`; `delldisplay codes` prints them.

Provenance: spec (MCCS, untested here), observed (read or written on this
panel), inferred (from DDPM's code or a capture), unknown.

## What the panel exposes

The monitor-side surface in this area is small:

| Code | What | Provenance |
|---|---|---|
| `0x62` | volume, 0-100 | spec (reads match the OSD; writes not checked by ear) |
| `0x8D` | OSD Speaker switch, `00` off / `01` on; not MCCS mute | observed |
| `0xCC` | OSD language, 8 values advertised | inferred (`02` English observed) |
| `0x04` | restore factory defaults, write `1` | spec |
| `0x05` | restore brightness/contrast, write `1` | spec |
| `0x08` | restore colour, write `1` | spec |

Everything else on DDPM's Audio, OSD and Personalize pages either rides on a
code this panel doesn't advertise or is host-side.

## Volume, 0x62

Continuous. The reply's `max` is authoritative (100 here). A GET reply was
captured from DDPM's traffic: current `0x0032`, max `0x0064`.

The unit has speakers. Reads match the OSD (Volume 50 reads 50), and the OSD
Speaker switch doesn't move it. Writes haven't been checked by ear.

Mask the value with `0x00FF` before showing it. Dell uses bits 14 and 15 of both
`0x62` and `0x8D` for OSD status (below). `delldisplay picture volume` masks.

## Speaker switch, 0x8D

On this panel `0x8D` isn't MCCS mute. It follows the OSD's Speaker switch:
Speaker On reads `0x01`, Speaker Off reads `0x00`, and `0x62` stays put either
way (observed). MCCS says `01` = mute and `02` = unmute, and DDPM assumes that
(its media-key path writes `2` when it reads `1`), so on this panel a `1` means
the opposite of what MCCS says. The reply has `max = 0x00FF`, so it doesn't
enumerate legal values.

The profile names the code `speaker`, with values `speaker-off` and
`speaker-on`, and `picture status` prints it that way. Value names come from
the profile, so `set audio-mute muted` is refused here rather than switching the
speaker on.
Panels whose profile keeps MCCS polarity still show `muted` / `unmuted`.

Writes are untested. `delldisplay` only writes `0x8D` when asked by name or
code with `set`. Don't read-modify-write: the upper bits carry status and
there is nothing up there worth echoing back.

### Mute isn't 0x8D

DDPM's "Speaker Mute" switch writes `0x62 = 0` and remembers the old level on
the host. Unmute writes the remembered level back. Its on/off state comes from
whether `0x62` reads zero. So the OSD Speaker switch and DDPM's mute are
different things and won't show up in each other's UI.

`delldisplay picture mute` follows DDPM: volume to zero plus a remembered level.
On unmute it re-reads `0x62` and reports a mismatch instead of restoring blindly,
since someone may have used the monitor's own keys in between.

DDPM also hides the mute switch on this panel: it only shows it when the
capability string lists `62(... FF ...)` or `62(... FE ...)`, and this panel
advertises bare `62`. That's a DDPM quirk, not a statement about the monitor.
Don't copy it.

### The extended 0x8D bitfield

On Dell panels with a microphone, `0x8D` packs four switches (inferred, read and
write paths agree):

- bit 14: audio enable
- bits 0-1: the MCCS mute field (`01` lights bit 0)
- bit 6: mic noise cancellation
- bit 7: mic beamforming

DDPM gates the mic group on `8D(... 01 ...)` in the capability string. This panel
advertises bare `8D` and has no mic. Not implemented.

## OSD status bits in 0x62 and 0x8D

DDPM decodes (inferred):

```
locked  = (0x62 & 0x8000) && (0x8D & 0x8000)
enabled = !(0x62 & 0x4000) && !(0x8D & 0x4000)
```

Both registers must agree. DDPM greys out its Audio page when "locked".

On this panel both high bytes read `0x00`. Nobody has seen them set. DDPM only
uses bit 14 for its tray icon when the capability string lists `8D(... C000 ...)`,
and this panel doesn't, which suggests the bits are dead here. But DDPM runs the
decoder above on every panel regardless.

`delldisplay` masks the volume and decodes the bits instead of throwing them
away.

## OSD language, 0xCC

Standard MCCS. The panel advertises `CC(02 03 04 06 09 0A 0D 0E)`. DDPM's table
matches MCCS 2.1. `0x0E` is Brazilian Portuguese; plain Portuguese (`0x08`) and
Chinese (`0x01`) aren't advertised. Only `02` = English has been read here.
Writes are untested.

## Restores, 0x04 / 0x05 / 0x08

Write-only, value `1`, no reply. All three advertised, none exercised here.

- `0x05` resets brightness and contrast.
- `0x08` resets colour.
- `0x04` resets everything: input, layout, OSD language, and probably the `0xE7`
  KVM association map (see [kvm.md](kvm.md)). Irreversible.

`delldisplay picture restore levels|colour|factory` needs `--i-mean-it` and reads
back the affected codes afterwards, since a SET is unacknowledged.

## Codes this panel doesn't have

DDPM has code paths for these. This panel refuses or doesn't advertise them:

| Code | DDPM use | Here |
|---|---|---|
| `0x63` | PiP/PBP audio source (`F1` main, `F2`-`F4` subs, `FE` USB only) | refused on the wire with result `0x01` (observed) |
| `0xCA` | OSD lock (`1` lock, `2` unlock) | not advertised |
| `0xAA` | screen orientation | not advertised |
| `0x69` | audio profile | not advertised |
| `0x94` | spatial audio | not advertised |
| `0x8F` `0x91` `0x93` `0x64` | treble, bass, balance, mic volume | not advertised, DDPM never touches them |

Don't write `0xCA` even as a probe. If a lock took, undoing it needs the
joystick.

The `0x63` refusal is the only negative here the panel said out loud. Its result
byte is `0x01`; `0xFE` is the HID tunnel's refusal, not DDC's. See [PROTOCOL.md](PROTOCOL.md).

## Host-side, no VCP

- DDPM's Speaker Mute switch (writes `0x62`, stores the level on the host).
- Audio Detect: follows the Mac's output device by writing `0x63`. Can't work
  here since `0x63` is refused.
- Fn and media-key behaviour, volume/mute hotkeys: host event taps. The action is
  a `0x62` or `0x8D` write.
- DDPM's Personalize tab (Menu Launcher, shortcut buttons). Not the monitor's
  own Personalize OSD menu; they share a name only.
- Power-button LED, OSD timer, transparency, OSD rotation, button lock, Fast
  Wakeup: joystick only. No register, no DDPM code path.

## Open questions

1. Does `0x8D` accept a write here? It's only been read. Test: write `0x00`,
   check the OSD shows Speaker Off, write `0x01`.
2. Do bits 14/15 of `0x62` / `0x8D` ever get set? Test: lock the OSD from the
   joystick, read both full 16-bit values, unlock, read again.
3. Does `0xCC` take a write? Test: write `0x03`, check the OSD renders French,
   write `0x02`.
4. Do the restores work? Cheapest test: change brightness, write `0x05 = 1`,
   read `0x10`. Only then think about `0x04`, after saving `0xE7` and `0xE9`.
5. Are `0xCA`, `0xAA`, `0x69`, `0x94` refused, or just unadvertised? One GET each
   should return result `0x01` or a Null Message.
