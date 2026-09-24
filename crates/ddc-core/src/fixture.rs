//! A U4323QE at rest, read off the hardware. Shared by tests across the workspace.
//!
//! Only built for tests, or with the `fixture` feature for other crates' tests.

use crate::caps::Capabilities;
use crate::guard::Snapshot;

/// The capability string this panel returns, byte for byte.
///
/// `DC(00 )` offers one picture mode, `60` lists five inputs, and `E9` lists
/// 13 values, 11 distinct (0x01 and 0x02 are write-aliases).
pub const U4323QE: &str = "(prot(monitor)type(lcd)model(U4323QE)cmds(01 02 03 07 0C E3 F3)vcp(02 04 05 08 10 12 14(04 05 06 08 09 0B 0C) 16 18 1A 52 60( 1B 0F 13 11 12) 62 8D AC AE B2 B6 C6 C8 C9 CC(02 03 04 06 09 0A 0D 0E) D6(01 04 05) DC(00 ) DF E0 E1 E2(00 0C 0D 0F 10 11 13 14) E5 E7(00 01 02 03) E8 E9(00 01 02 21 22 24 2F 31 32 33 34 35 41) EE EF F1 F2 FE FD)mccs_ver(2.1)mswhql(1))";

/// The seven registers that define the panel's resting state.
///
/// 2-up self-left (`0xE9 = 0x24`), main on USB-C, `0xE8 = 0x6DF1` =
/// [hdmi1, dp1, usb-c], KVM map `0x2540`, USB inventory `0xBA98`, OSD idle,
/// and `0xF1 = 0xC12B`.
pub const IDLE_READS: &[(u8, u16)] = &[
    (0xF2, 0x0000),
    (0xF1, 0xC12B),
    (0xE9, 0x0024),
    (0x60, 0x1B1B),
    (0xEE, 0xBA98),
    (0xE7, 0x2540),
    (0xE8, 0x6DF1),
];

/// The parsed capability string.
pub fn caps() -> Capabilities {
    Capabilities::parse(U4323QE)
}

/// A [`Snapshot`] of the panel at rest.
pub fn idle_snapshot() -> Snapshot {
    Snapshot::from_reads(crate::vcp::default_panel(), caps(), IDLE_READS)
}
