//! Which machine holds the monitor's USB hub, guessed from the USB tree.
//!
//! The monitor does not report upstream ownership over DDC, so this counts
//! the hub's devices instead. It is a heuristic, not a protocol query.

use super::{each_service, property, Cf};

/// USB vendor id of the Microchip hub silicon inside Dell UltraSharp monitors.
pub const MICROCHIP_VENDOR: u16 = 0x0424;

/// Hub devices visible when this Mac owns the upstream. Fewer means the KVM
/// has handed USB to the other machine.
const HERE_THRESHOLD: usize = 3;

/// Count attached USB devices from `vendor_id`.
pub fn usb_device_count(vendor_id: u16) -> usize {
    let key = Cf::string("idVendor");
    let mut n = 0;
    each_service("IOUSBHostDevice", |dev| {
        if property(dev, &key)
            .as_i32()
            .is_some_and(|v| v as u16 == vendor_id)
        {
            n += 1;
        }
    });
    n
}

/// `(hub device count, whether USB is attached to this Mac)`.
pub fn usb_here() -> (usize, bool) {
    let n = usb_device_count(MICROCHIP_VENDOR);
    (n, n >= HERE_THRESHOLD)
}
