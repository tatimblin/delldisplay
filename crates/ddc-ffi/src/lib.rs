//! C ABI over the DDC session. See `include/delldisplay.h`, which documents
//! blocking, threading and return codes.

use std::ffi::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};

use ddc_transport::{Ddc, Error, I2c};

pub const DD_OK: c_int = 0;
pub const DD_ERR_ARG: c_int = -1;
pub const DD_ERR_IO: c_int = -2;
pub const DD_ERR_REFUSED: c_int = -3;
pub const DD_ERR_UNSUPPORTED: c_int = -4;
pub const DD_ERR_PANIC: c_int = -5;

#[cfg(target_os = "macos")]
mod platform {
    pub use ddc_transport::macos::AvService as Transport;

    pub fn count() -> Option<usize> {
        Some(Transport::count())
    }

    pub fn open(index: usize) -> Option<Transport> {
        Transport::open(index).ok()
    }
}

/// No transport off macOS: nothing opens, so every handle call sees NULL.
#[cfg(not(target_os = "macos"))]
mod platform {
    use ddc_transport::{Error, I2c};

    pub enum Transport {}

    impl I2c for Transport {
        fn write(&mut self, _: u8, _: u8, _: &[u8]) -> Result<(), Error> {
            match *self {}
        }
        fn read(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), Error> {
            match *self {}
        }
    }

    pub fn count() -> Option<usize> {
        None
    }

    pub fn open(_: usize) -> Option<Transport> {
        None
    }
}

pub struct DdHandle(Ddc<platform::Transport>);

fn code(e: &Error) -> c_int {
    match e {
        Error::Refused { .. } => DD_ERR_REFUSED,
        Error::Io { .. } | Error::NoReply { .. } | Error::NotFound => DD_ERR_IO,
    }
}

/// Run `f` so a panic becomes `DD_ERR_PANIC` instead of unwinding into C.
fn guarded(f: impl FnOnce() -> c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(DD_ERR_PANIC)
}

/// [`guarded`], with `h` checked for NULL and borrowed.
///
/// # Safety
/// `h` must be NULL or a live handle from `dd_open`, not in use elsewhere.
unsafe fn with_handle(
    h: *mut DdHandle,
    f: impl FnOnce(&mut Ddc<platform::Transport>) -> c_int,
) -> c_int {
    guarded(|| match unsafe { h.as_mut() } {
        Some(h) => f(&mut h.0),
        None => DD_ERR_ARG,
    })
}

/// Number of external displays, or a negative error.
#[no_mangle]
pub extern "C" fn dd_count() -> c_int {
    guarded(|| match platform::count() {
        Some(n) => c_int::try_from(n).unwrap_or(c_int::MAX),
        None => DD_ERR_UNSUPPORTED,
    })
}

/// A session with its own profile, picked from the EDID model.
fn session<T: I2c>(t: T) -> Ddc<T> {
    let mut d = Ddc::new(t);
    d.detect_panel();
    d
}

/// Open display `index`, with the profile its EDID model picks. NULL on failure.
#[no_mangle]
pub extern "C" fn dd_open(index: usize) -> *mut DdHandle {
    catch_unwind(|| platform::open(index).map(session))
        .ok()
        .flatten()
        .map_or(std::ptr::null_mut(), |d| {
            Box::into_raw(Box::new(DdHandle(d)))
        })
}

/// Free a handle from [`dd_open`]. NULL is ignored.
///
/// # Safety
/// `h` must be NULL or come from `dd_open`, and must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn dd_close(h: *mut DdHandle) {
    if !h.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(h) })));
    }
}

/// Turn the double-write policy on (1, the default) or off.
///
/// # Safety
/// `h` must be NULL or a live handle from `dd_open`.
#[no_mangle]
pub unsafe extern "C" fn dd_set_double_write(h: *mut DdHandle, on: c_int) -> c_int {
    with_handle(h, |d| {
        d.policy.double_write = on != 0;
        DD_OK
    })
}

/// Read a VCP feature into `current` / `max`.
///
/// # Safety
/// `h` must be NULL or live; `current` and `max` must be NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn dd_get(
    h: *mut DdHandle,
    vcp: u8,
    current: *mut u16,
    max: *mut u16,
) -> c_int {
    with_handle(h, |d| match d.get(vcp) {
        Ok(r) => {
            unsafe {
                if !current.is_null() {
                    *current = r.current;
                }
                if !max.is_null() {
                    *max = r.max;
                }
            }
            DD_OK
        }
        Err(e) => code(&e),
    })
}

/// Write a VCP feature.
///
/// # Safety
/// `h` must be NULL or a live handle from `dd_open`.
#[no_mangle]
pub unsafe extern "C" fn dd_set(h: *mut DdHandle, vcp: u8, value: u16) -> c_int {
    with_handle(h, |d| match d.set(vcp, value) {
        Ok(()) => DD_OK,
        Err(e) => code(&e),
    })
}

/// Copy the capabilities string into `buf`, snprintf-style: truncate to fit,
/// always NUL-terminate, return the full length.
///
/// # Safety
/// `h` must be NULL or live; `buf` must be writable for `len` bytes, or NULL
/// with `len == 0`.
#[no_mangle]
pub unsafe extern "C" fn dd_capabilities(h: *mut DdHandle, buf: *mut c_char, len: usize) -> c_int {
    if buf.is_null() && len != 0 {
        return DD_ERR_ARG;
    }
    with_handle(h, |d| match d.capabilities() {
        Ok(c) => {
            let bytes = c.raw.as_bytes();
            if len > 0 {
                let n = bytes.len().min(len - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
                    *buf.add(n) = 0;
                }
            }
            c_int::try_from(bytes.len()).unwrap_or(c_int::MAX)
        }
        Err(e) => code(&e),
    })
}

#[cfg(test)]
mod header {
    //! The header is hand-written; these tests fail when it drifts from Rust.

    use super::*;
    use ddc_core::vcp::{input, pip, Vcp};
    use std::collections::{BTreeMap, BTreeSet};

    const HEADER: &str = include_str!("../include/delldisplay.h");
    const SOURCE: &str = include_str!("lib.rs");

    /// `#define DD_* value` lines, value parsed as hex or signed decimal.
    fn defines() -> BTreeMap<String, i64> {
        HEADER
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                (w.next()? == "#define").then_some(())?;
                let name = w.next()?;
                name.starts_with("DD_").then_some(())?;
                let v = w.next()?.trim_matches(|c| c == '(' || c == ')');
                let n = match v.strip_prefix("0x") {
                    Some(hex) => i64::from_str_radix(hex, 16).ok()?,
                    None => v.parse().ok()?,
                };
                Some((name.to_string(), n))
            })
            .collect()
    }

    #[test]
    fn every_define_matches_rust() {
        let want: BTreeMap<String, i64> = [
            ("DD_OK", DD_OK as i64),
            ("DD_ERR_ARG", DD_ERR_ARG as i64),
            ("DD_ERR_IO", DD_ERR_IO as i64),
            ("DD_ERR_REFUSED", DD_ERR_REFUSED as i64),
            ("DD_ERR_UNSUPPORTED", DD_ERR_UNSUPPORTED as i64),
            ("DD_ERR_PANIC", DD_ERR_PANIC as i64),
            ("DD_VCP_BRIGHTNESS", Vcp::BRIGHTNESS as i64),
            ("DD_VCP_CONTRAST", Vcp::CONTRAST as i64),
            ("DD_VCP_INPUT", Vcp::INPUT_SOURCE as i64),
            ("DD_VCP_VOLUME", Vcp::VOLUME as i64),
            ("DD_VCP_POWER", Vcp::POWER_MODE as i64),
            ("DD_VCP_PIP", Vcp::PIP_MODE as i64),
            ("DD_INPUT_DP", input::DISPLAY_PORT as i64),
            ("DD_INPUT_HDMI1", input::HDMI_1 as i64),
            ("DD_INPUT_HDMI2", input::HDMI_2 as i64),
            ("DD_INPUT_DP2", input::DP2 as i64),
            ("DD_INPUT_USBC", input::USB_C as i64),
            ("DD_PIP_OFF", pip::OFF as i64),
            ("DD_PIP_ON", pip::PIP_SMALL as i64),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(defines(), want);
    }

    /// Header prototypes, whitespace-normalised, keyed by function name.
    fn prototypes() -> BTreeMap<String, String> {
        HEADER
            .lines()
            .filter(|l| l.contains("dd_") && l.trim_end().ends_with(");"))
            .map(|l| {
                let p = l.split_whitespace().collect::<Vec<_>>().join(" ");
                let name = p
                    .split('(')
                    .next()
                    .unwrap()
                    .rsplit([' ', '*'])
                    .next()
                    .unwrap();
                (name.to_string(), p)
            })
            .collect()
    }

    #[test]
    fn every_export_is_declared_with_the_right_signature() {
        // The fn-pointer coercions pin the Rust side; the strings pin the C side.
        type H = *mut DdHandle;
        let _: extern "C" fn() -> c_int = dd_count;
        let _: extern "C" fn(usize) -> H = dd_open;
        let _: unsafe extern "C" fn(H) = dd_close;
        let _: unsafe extern "C" fn(H, c_int) -> c_int = dd_set_double_write;
        let _: unsafe extern "C" fn(H, u8, *mut u16, *mut u16) -> c_int = dd_get;
        let _: unsafe extern "C" fn(H, u8, u16) -> c_int = dd_set;
        let _: unsafe extern "C" fn(H, *mut c_char, usize) -> c_int = dd_capabilities;
        let want: BTreeMap<String, String> = [
            ("dd_count", "int dd_count(void);"),
            ("dd_open", "DdHandle *dd_open(size_t index);"),
            ("dd_close", "void dd_close(DdHandle *h);"),
            (
                "dd_set_double_write",
                "int dd_set_double_write(DdHandle *h, int on);",
            ),
            (
                "dd_get",
                "int dd_get(DdHandle *h, uint8_t vcp, uint16_t *current, uint16_t *max);",
            ),
            (
                "dd_set",
                "int dd_set(DdHandle *h, uint8_t vcp, uint16_t value);",
            ),
            (
                "dd_capabilities",
                "int dd_capabilities(DdHandle *h, char *buf, size_t len);",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(prototypes(), want);

        // And nothing is exported that the list above forgot.
        let exported: BTreeSet<&str> = SOURCE
            .lines()
            .filter_map(|l| l.split(r#"extern "C" fn "#).nth(1))
            .filter_map(|rest| rest.split('(').next())
            .filter(|n| n.starts_with("dd_"))
            .collect();
        assert_eq!(exported, want.keys().map(String::as_str).collect());
    }

    #[test]
    fn null_handles_are_argument_errors() {
        let h = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                dd_get(h, 0x10, std::ptr::null_mut(), std::ptr::null_mut()),
                DD_ERR_ARG
            );
            assert_eq!(dd_set(h, 0x10, 50), DD_ERR_ARG);
            assert_eq!(dd_set_double_write(h, 1), DD_ERR_ARG);
            assert_eq!(dd_capabilities(h, std::ptr::null_mut(), 0), DD_ERR_ARG);
            dd_close(h);
        }
    }

    #[test]
    fn a_panic_becomes_an_error_code() {
        assert_eq!(guarded(|| panic!("boom")), DD_ERR_PANIC);
    }
}
