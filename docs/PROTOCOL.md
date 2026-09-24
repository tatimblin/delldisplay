# DDC/CI protocol notes (Dell U4323QE)

Measured on a U4323QE over DisplayPort from Apple Silicon. Other panels differ;
see their profile in `crates/ddc-panels/profiles/`. The feature docs link here
for wire details instead of repeating them.

## Transport

DDC/CI over the DisplayPort AUX channel, through the private `IOAVService` API in
`CoreDisplay.framework`:

    -framework IOKit -framework CoreDisplay -F/System/Library/PrivateFrameworks

Find the service by walking `DCPAVServiceProxy` nodes in the IORegistry, take the
one with `Location == "External"`, and pass it to `IOAVServiceCreateWithService`.
DDPM uses the same calls. EDID reads work at chip `0x50`.

DDC/CI has to be on in the OSD (Menu › Others › DDC/CI). With it off the panel
still serves EDID but answers every command with the Null Message.

## Framing

Standard MCCS. Nothing vendor-specific about the frame itself.

Write to chip `0x37`, offset `0x51`:

    GET:   82 01 <code> <ck>
    SET:   84 03 <code> <hi> <lo> <ck>
    CAPS:  83 F3 <offHi> <offLo> <ck>

Request checksum: XOR of `0x6E`, `0x51` and every frame byte.

Read the reply from chip `0x37`, offset `0x00`. A GET reply is 11 bytes:

    [0] 6E  [1] 88  [2] 02  [3] result  [4] code
    [5] type  [6..7] max (hi, lo)  [8..9] current (hi, lo)  [10] ck

Reply checksum: XOR of `0x50` and bytes `[0 .. 2+len)`, where
`len = [1] & 0x7F`. Values are 16-bit; most codes use only the low byte.

## Send every request twice

DDPM writes each request frame twice, identically, before reading. A single write
on this panel gets the Null Message back nearly every time. With the double write,
replies come back reliably.

    write(frame); sleep 40 ms
    write(frame); sleep 40 ms   # same bytes again
    read(reply)

This applies to action registers too. A single `84 03 E5 F0 10 BD` does nothing;
the doubled frame swaps. In code it is `Policy::double_write` (on by default,
`--no-double-write` on the CLI).

## Reply rules

- Result byte `0x00` is ok. `0x01` means refused or unsupported (for example
  `0x63` audio source on this panel). The USB-HID tunnel below uses `0xFE`
  instead; don't mix them up.
- The type byte lies. The panel reports enumerated codes like `0x60` as
  continuous and sometimes mirrors the value into both bytes (`0x1B1B`). The
  profile's `enumerated` list decides, not the wire.
- Bytes past the declared length are stale buffer content from an earlier reply.
  Never read past `len`.
- With DDC/CI off, or when the panel isn't ready, you get the Null Message
  `6E 80 BE`. Retry; don't treat it as a value.
- The panel ACKs writes it ignores. `0xDC` accepts unadvertised picture modes and
  stays at `0x00`; `0xE5` zoom (`0x0002`) and underscan (`0x0003`) are accepted and
  do nothing. A successful write proves nothing here, so read back after writes
  wherever a readback exists.

## Timing

- 40 ms before, between and after the doubled write (MCCS asks for at least 40 ms
  between request and reply).
- Post-write settle: 60 ms, or 150 ms for `0xD6`, `0xE0`, `0xE1`.
- Layout (`0xE9`) and input (`0x60`) writes make the panel re-sync. The
  `IOAVService` handle can go stale (`I2c::reconnect()` reopens it) and the
  registry node can briefly disappear (`open` retries). Some layouts also change
  the active input, so snapshot `0x60` if you care about it.
- Right after a layout change the capabilities read can go unanswered for about
  a second. `Ddc::capabilities` starts the read over with backoff, for about 3 s
  in total, before it gives up.
- `0xF2` bit 7 set means the panel is busy (OSD open, mid-operation). Gate writes
  on it.

## Capabilities string

Read via `0xF3`. This panel returns:

    prot(monitor) type(lcd) model(U4323QE)
    cmds(01 02 03 07 0C E3 F3)
    vcp(02 04 05 08 10 12 14(04 05 06 08 09 0B 0C) 16 18 1A 52
        60(1B 0F 13 11 12) 62 8D AC AE B2 B6 C6 C8 C9
        CC(02 03 04 06 09 0A 0D 0E) D6(01 04 05) DC(00) DF E0 E1
        E2(00 0C 0D 0F 10 11 13 14) E5 E7(00 01 02 03) E8
        E9(00 01 02 21 22 24 2F 31 32 33 34 35 41) EE EF F1 F2 FE FD)
    mccs_ver(2.1) mswhql(1)

`0xCA` (OSD lock) is not advertised.

## The codes that matter

Values and names live in `crates/ddc-panels/profiles/dell-u4323qe.toml` and are
printed by `delldisplay codes`. In short:

| Code | What it is | Doc |
|------|------------|-----|
| `0x10` / `0x12` | brightness / contrast, 0-100 | [picture.md](picture.md) |
| `0x14` | `color-preset` (writes work; value names unverified) | [picture.md](picture.md) |
| `0x60` | input: `0x1B` usb-c, `0x0F` dp1, `0x13` dp2, `0x11` hdmi1, `0x12` hdmi2 | [input.md](input.md) |
| `0x62` / `0x8D` | volume / OSD Speaker switch (`00` off, `01` on; not MCCS mute) | [audio-osd.md](audio-osd.md) |
| `0xD6` | power: `01` on, `04` standby, `05` off | [system.md](system.md) |
| `0xE2` | `preset-mode`, a preset readback; relationship to `0x14` unknown | [picture.md](picture.md) |
| `0xE5` | PxP action: `0xF010` swaps main and sub1 | [pxp.md](pxp.md) |
| `0xE7` | USB-KVM association register; `0xFF00` toggles the upstream | [kvm.md](kvm.md) |
| `0xE8` | PxP sub-sources, three packed 5-bit input codes | [pxp.md](pxp.md) |
| `0xE9` | PiP/PBP layout, 11 distinct layouts | [pxp.md](pxp.md) |
| `0xEE` | USB upstream port inventory, read-only | [kvm.md](kvm.md) |
| `0xF2` | status; bit 7 = busy | [system.md](system.md) |

## Dead ends

- The Microchip `0424:7260` HID tunnel that Dell's SDK dylib uses reaches only the
  USB hub MCU. It answers identity commands (`0x01` name, `0x06` service tag,
  `0xA1` internal version) and returns `0xFE` for session setup (`0x0D`) and every
  control command. It is not the scaler. The frames here were captured from
  DDPM's own traffic.
- Malformed vendor HID reports can wedge the hub firmware. It happened once while
  probing the tunnel: the whole hub tree (keyboard receiver, webcam) dropped and
  only a replug or power-cycle brought it back. Don't probe the hub while your
  input devices hang off it.
- The SDK's `0xEB` vendor command table is a separate namespace from VCP codes. A
  name there says nothing about a VCP code.
