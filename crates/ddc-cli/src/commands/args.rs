//! Flags and value parsers shared by several commands.
//!
//! Anything that can be checked without the panel is checked here, so a bad
//! name is a usage error (exit 2) before the display is even opened. Names
//! come from the default profile for the same reason.

use clap::Args;
use ddc_core::pxp::{self, Window};
use ddc_core::vcp::{self, Vcp};

/// Flags every writing verb takes.
#[derive(Args, Debug, Clone, Default)]
pub struct Write {
    /// Read and check, print what would be written, write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Cap every settle wait at this many milliseconds.
    #[arg(long, value_name = "MS")]
    pub settle_ms: Option<u64>,
}

/// A VCP code: a name from `delldisplay codes`, or hex like `0x10` or `10`.
pub fn code(s: &str) -> Result<u8, String> {
    vcp::resolve(vcp::default_panel(), s)
        .ok_or_else(|| format!("unknown VCP code '{s}' (a name from `codes`, or 0xNN)"))
}

/// A byte: `0x` for hex, decimal otherwise.
pub fn byte(s: &str) -> Result<u8, String> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => t.parse(),
    }
    .map_err(|_| format!("'{s}' isn't a byte (decimal, or 0x for hex)"))
}

/// An input source: dp, dp2, hdmi1, hdmi2, usb-c, or 0xNN.
pub fn input(s: &str) -> Result<u8, String> {
    match vcp::resolve_value(vcp::default_panel(), Vcp::INPUT_SOURCE, s) {
        Some(v) if v <= 0xFF => Ok(v as u8),
        _ => Err(format!(
            "unknown input '{s}' (dp, dp2, hdmi1, hdmi2, usb-c or 0xNN)"
        )),
    }
}

/// A PiP/PBP layout: a name like `pip` or `quad`, or 0xNN.
pub fn layout(s: &str) -> Result<u8, String> {
    match vcp::resolve_value(vcp::default_panel(), Vcp::PIP_MODE, s) {
        Some(v) if v <= 0xFF => Ok(v as u8),
        _ => Err(format!("unknown layout '{s}' (see `pxp layouts`, or 0xNN)")),
    }
}

/// A window: main, sub1, sub2, sub3, or 0-3.
pub fn window(s: &str) -> Result<Window, String> {
    s.parse()
}

/// A sub-window. The main window's source is 0x60, set with `set input`.
pub fn sub_window(s: &str) -> Result<Window, String> {
    match window(s)? {
        w if w.is_main() => Err(String::from(
            "the main window's source is 0x60; use `delldisplay set input <name>`",
        )),
        w => Ok(w),
    }
}

/// A PIP inset corner, as its index into [`pxp::INSET_CORNERS`].
pub fn corner(s: &str) -> Result<u8, String> {
    let n = s.trim().to_ascii_lowercase();
    pxp::INSET_CORNERS
        .iter()
        .position(|c| *c == n)
        .map(|i| i as u8)
        .ok_or_else(|| format!("unknown corner '{s}' ({})", pxp::INSET_CORNERS.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_must_fit_in_a_byte() {
        assert_eq!(input("usb-c"), Ok(0x1B));
        assert_eq!(input("0x0F"), Ok(0x0F));
        assert!(input("0x1FF").is_err());
        assert!(input("vga").is_err());
    }

    #[test]
    fn bytes_are_decimal_unless_marked_hex() {
        assert_eq!(byte("0x21"), Ok(0x21));
        assert_eq!(byte("33"), Ok(33));
        assert!(byte("zz").is_err());
        assert!(byte("256").is_err());
    }
}
