# USB KVM and upstream selection

Measured on a U4323QE; other panels differ, see their profile.

How the panel's built-in USB hub moves between attached hosts. The monitor side
is three codes: 0xE7 (USB-KVM association register, plus write-only actions),
0xEE (upstream port inventory, read-only) and 0x60 (main input; see
[input.md](input.md)). 0xE8 and 0xE9 show up because DDPM's KVM wizard writes
them; they are covered in [input.md](input.md) and [pxp.md](pxp.md).

Framing, checksums, the double-write and the reply shape are in
[PROTOCOL.md](PROTOCOL.md). Code names and values are in
`crates/ddc-panels/profiles/dell-u4323qe.toml`, or run `delldisplay codes`.

Provenance: spec (MCCS), observed (read or written on this panel), inferred
(from DDPM's code or a capture, not exercised here), unknown.

Cites are to DDPM 2.3.0.1005 and private captures, not in this repo.

Careful: a keyboard and mouse plugged into the monitor's hub go wherever USB
goes. A wrong 0xE7 write can leave you with no input on the machine you're
sitting at. Have another keyboard, or Bluetooth, before experimenting.

## 0xE7 is three things on one number

1. Toggle action: `0xFF00`.
2. Window-bind action: `0xFF01`-`0xFF04`.
3. Association map: every other value.

A driver that treats 0xE7 as one scalar will get this wrong.

### Toggle, 0xFF00

Moves the hub to the next upstream. Observed: this Mac's USB tree went 18
devices, then 6, then 18 across two toggles.

It is an action. The register keeps reading its previous value afterwards, so
reading 0xE7 can't tell you where USB went. The only oracle is the host's own
USB tree (on macOS, devices under the Microchip hub, vendor 0x0424).
`delldisplay kvm usb` reports that, and `kvm ensure here|away` is the
idempotent form; a retried bare toggle flips USB straight back.

### Window bind, 0xFF01-0xFF04

Bind USB to the host shown in PxP window 1-4. Inferred from DDPM, never
confirmed here.

DDPM only offers it when `FF` appears in the capability string's `E7(...)` list.
This panel advertises `E7(00 01 02 03)`, no `FF`. `delldisplay` applies the same
gate and refuses (exit 3, nothing written). Whether the panel itself would also
reject the write is untested.

### Association map

A stored read/write map of input to the upstream port the panel attaches when
that input is active. Writing it switches nothing on its own.

Observed:

- The writable window is bits 6-13 (mask 0x3FC0). Writes to bits 0-5 and 14-15
  are dropped without complaint.
- Bits 6-13 hold four 2-bit fields.
- The baseline reads 0x2540. By bit position (bit 6 first) that is
  `[1, 1, 1, 2]`.
- This Mac is on dp2. Only the field at bits 10-11 set to 1 makes a toggle
  return USB here.

Inferred: a field value is an index into the 0xEE inventory, not an absolute
port code. DDPM indexes into 0xEE, and 0xEE here has no USB-B slot, so a
fixed "1 = USB-B2" reading could not be represented.

Unknown: which input owns which field. Two models fit every measurement:

| Model | Input 0 sits at | Later inputs | Status |
|---|---|---|---|
| Low-first | bit 6 | climb by 2 | shipped (`FieldOrder::LowFirst`) |
| High-first (DDPM's encoder) | bit 14 | descend by 2 | not shipped |

With the panel's input list `60(1B 0F 13 11 12)`, both put input index 2 (dp2)
at bit 10, and that is the only index ever tested. So the measurements so far
can't separate them. DDPM's own classifier (capability string contains `EE`)
would pick high-first for this panel, which is a real argument against the
shipped default. `ddc_core::kvm::FIELD_ORDER` is the single constant to flip,
and `delldisplay` says which model it is using when it writes.

Under low-first, hdmi2 (index 4) has no writable field. Under high-first,
usb-c (index 0) lands on bit 14, which is masked.

DDPM also has a legacy encoding for panels without `EE`: fields ascend from
bit 0 and hold absolute port codes. Not implemented; this panel doesn't use it.

Always read-modify-write. Writing the map without the current word clobbers
every other input's association. `delldisplay kvm associate` refuses if 0xE7
wasn't read.

DDPM takes a no-read-back write path for 0xE7 on this exact model. Don't poll
0xE7 until it matches what you wrote: the action values never match, and the
map may not either. `delldisplay` reads it back to report, not to decide
success. Inferred.

## 0xEE: port inventory (read-only)

Four 4-bit port-type IDs, slot 0 in the low nibble. Reads 0xBA98 here, so slots
0-3 are 8, 9, A, B. Observed.

DDPM's names for the nibbles: 0 USB-B, 1 USB-B2, 8 USB-C, 9 USB-C2, A USB-C3,
B USB-C4, C Thunderbolt, D Thunderbolt2. That makes this panel four USB-C-family
slots. Inferred; which physical connector slot 0 is has not been checked.

DDPM never writes 0xEE.

## DDPM's KVM wizard

The wizard commit writes, one second apart:

```
0x60 -> 0xE8 -> 0xE9 -> 0xE8 -> 0xE7 -> 0xE8
```

0xE8 goes three times because it and 0xE9 constrain each other. Each write is
observed on its own; that the order matters is inferred. `delldisplay kvm
commit` follows it.

DDPM's "switch between PCs" hotkey reads 0x60, picks the next host from its own
settings, and writes 0x60. It never touches 0xE7; the panel is expected to move
USB because of the association map. Inferred, and untested here.

DDPM decides between a toggle-only UI and a window picker from the 0xE9 layout:
0x21-0x2F and 0x51 count as 2 hosts, 0x31-0x36 as 3, 0x41-0x42 as 4, everything
else 1. It's a UI heuristic across all Dell models. It matches this panel's
settled pane counts for 0x32, 0x33 and 0x35 (three each).

## Not on this panel

- 0xEA, USB-C prioritization. Not advertised.
- The HID tunnel's `0xEB` command table has an `0xAA USBAssociation` entry.
  That table is a separate namespace and only reaches the hub MCU; see
  [PROTOCOL.md](PROTOCOL.md). It says nothing about VCP 0xE7.

## Host-side, no VCP

- Dell Network KVM. Host-to-host over the network (PIN pairing, clipboard, file
  transfer). No DDC call site belongs to it.
- KVM mode selector and per-PC nicknames. DDPM settings JSON.
- PBP mouse crossover. DDPM watches the pointer cross a pane edge and writes
  0xE7 = 0xFF0N. Its table starts at 0xE9 = 0x23, so PiP layouts 0x21/0x22 get
  no crossover. Only the resulting write touches the panel.
- The window-count gate and the `EE` classifier above. Both are host decisions
  about which write to send.

## Open questions

- Field order. Associate a different input (not dp2) with this host's upstream,
  select that input, and see whether USB follows. That separates low-first from
  high-first.
- Does the map accept writes that stick? Write 0x2440 (`84 03 E7 24 40 3B`), then
  read once. Here a readback is diagnostic, not a verify loop.
- What slot 0 of 0xEE physically is. Move one field between slots and see which
  upstream cable the host's USB appears on.
- Does 0x60 alone move USB, given a programmed map? Change input with no 0xE7
  write and watch the host's USB tree.
- Does the panel itself reject 0xFF01-0xFF04, or only DDPM? Would need a
  deliberate write past the guard.
- Does the wizard's order matter? Apply a config in reverse and read back.
