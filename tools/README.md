# tools

Small C and Objective-C programs for working out the protocol. They aren't part
of the library or the CLI, and most people never need them.

All of them need Dell Display and Peripheral Manager (DDPM) installed at
`/Applications/DDPM`, and the Xcode command line tools. They load Dell's SDK
dylib from the installed app at runtime; nothing from Dell is in this repo.

| Tool | What it does |
|---|---|
| `harness/sdkharness` | Loads Dell's SDK and calls its read-only getters (SDK version, monitor list, name, service tag, input, PxP mode). Every byte on the wire comes from Dell's code. It prints your service tag; redact it before sharing output. |
| `harness/hidlog.dylib` | Injected into `sdkharness` with `DYLD_INSERT_LIBRARIES` to print the HID reports going out and coming back. |
| `harness/session` | Asks the SDK to start a session, which on some panels shows an approval prompt on the monitor, then calls the privileged getters. On the U4323QE the hub refuses the session. |
| `probe/hidprobe` | Scans for vendor HID devices (Dell or Microchip) and sends a few read-shaped DDC frames through the HID-to-I2C tunnel. |

Apart from the session token `session` sends, these only send reads. They still talk to the monitor's USB hub MCU, and
malformed reports to it have wedged the hub before. Run them only with a
keyboard and mouse that don't go through the monitor, and expect to replug or
power-cycle if the hub drops. See [docs/PROTOCOL.md](../docs/PROTOCOL.md).

## Build

```sh
tools/harness/build.sh
clang -fobjc-arc -framework Foundation -framework IOKit \
    tools/probe/hidprobe.m -o tools/probe/hidprobe
```

Then:

```sh
cd tools/harness
DYLD_INSERT_LIBRARIES=./hidlog.dylib ./sdkharness
```

Built binaries are gitignored.
