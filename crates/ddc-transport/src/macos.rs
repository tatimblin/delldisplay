//! macOS transport via the private `IOAVService` API in IOKit.
//!
//! The only I2C path on Apple Silicon; the Intel-era `IOFramebuffer`
//! interface is not available there.

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::time::Duration;

use crate::{Error, I2c};

mod usb;
pub use usb::{usb_device_count, usb_here, MICROCHIP_VENDOR};

type IOReturn = i32;
type IoObject = u32;
type CFTypeRef = *const c_void;
type CFTypeID = usize;

const KERN_SUCCESS: IOReturn = 0;
/// `kIOMainPortDefault`.
const MAIN_PORT: u32 = 0;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_COMPARE_EQUAL_TO: isize = 0;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFTypeRef;
    fn IOServiceGetMatchingServices(port: u32, matching: CFTypeRef, it: *mut u32) -> IOReturn;
    fn IOIteratorNext(it: u32) -> IoObject;
    fn IOObjectRelease(obj: IoObject) -> IOReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFTypeRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryGetRegistryEntryID(entry: IoObject, id: *mut u64) -> IOReturn;

    fn IOAVServiceCreateWithService(allocator: CFTypeRef, service: IoObject) -> CFTypeRef;
    fn IOAVServiceReadI2C(
        s: CFTypeRef,
        chip: u32,
        offset: u32,
        buf: *mut c_void,
        len: u32,
    ) -> IOReturn;
    fn IOAVServiceWriteI2C(
        s: CFTypeRef,
        chip: u32,
        offset: u32,
        buf: *const c_void,
        len: u32,
    ) -> IOReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFStringCreateWithCString(alloc: CFTypeRef, cstr: *const c_char, encoding: u32)
        -> CFTypeRef;
    fn CFStringCompare(a: CFTypeRef, b: CFTypeRef, options: usize) -> isize;
    fn CFNumberGetValue(num: CFTypeRef, the_type: isize, value_ptr: *mut c_void) -> u8;
}

/// An owned CF object, released on drop. May hold NULL.
struct Cf(CFTypeRef);

impl Cf {
    fn string(s: &str) -> Cf {
        let c = CString::new(s).expect("no interior NUL");
        Cf(unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8) })
    }

    /// Whether this is a CFString equal to `other`.
    fn string_eq(&self, other: &Cf) -> bool {
        if self.0.is_null() || other.0.is_null() {
            return false;
        }
        unsafe {
            CFGetTypeID(self.0) == CFStringGetTypeID()
                && CFStringCompare(self.0, other.0, 0) == CF_COMPARE_EQUAL_TO
        }
    }

    /// The value as an i32, if this is a CFNumber.
    fn as_i32(&self) -> Option<i32> {
        const K_CF_NUMBER_SINT32: isize = 3;
        if self.0.is_null() || unsafe { CFGetTypeID(self.0) != CFNumberGetTypeID() } {
            return None;
        }
        let mut v: i32 = 0;
        let ok =
            unsafe { CFNumberGetValue(self.0, K_CF_NUMBER_SINT32, (&mut v as *mut i32).cast()) };
        (ok != 0).then_some(v)
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Read a registry property. The result may hold NULL.
fn property(entry: IoObject, key: &Cf) -> Cf {
    if key.0.is_null() {
        return Cf(ptr::null());
    }
    Cf(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, ptr::null(), 0) })
}

/// Call `f` on every registry entry of IOKit class `class`, releasing each after.
fn each_service(class: &str, mut f: impl FnMut(IoObject)) {
    let class = CString::new(class).expect("no interior NUL");
    let mut it: u32 = 0;
    unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        // IOServiceGetMatchingServices consumes `matching`.
        if matching.is_null()
            || IOServiceGetMatchingServices(MAIN_PORT, matching, &mut it) != KERN_SUCCESS
        {
            return;
        }
        loop {
            let obj = IOIteratorNext(it);
            if obj == 0 {
                break;
            }
            f(obj);
            IOObjectRelease(obj);
        }
        IOObjectRelease(it);
    }
}

/// Visit every `DCPAVServiceProxy` whose `Location` is `External`, in registry
/// order, as `(registry entry id, service)`.
fn each_external(mut f: impl FnMut(u64, IoObject)) {
    let key = Cf::string("Location");
    let want = Cf::string("External");
    each_service("DCPAVServiceProxy", |svc| {
        if property(svc, &key).string_eq(&want) {
            let mut id = 0u64;
            unsafe { IORegistryEntryGetRegistryEntryID(svc, &mut id) };
            f(id, svc);
        }
    });
}

/// Which external service to open.
#[derive(Clone, Copy)]
enum Target {
    Index(usize),
    Entry(u64),
}

/// Open the external service matching `target`, once, with no waiting.
///
/// Returns the handle, its entry id and how many external services exist.
fn open_once(target: Target) -> Result<(CFTypeRef, u64, usize), Error> {
    let mut found: Option<Result<(CFTypeRef, u64), Error>> = None;
    let mut n = 0usize;
    each_external(|id, svc| {
        let hit = match target {
            Target::Index(i) => i == n,
            Target::Entry(want) => want == id,
        };
        if hit && found.is_none() {
            let av = unsafe { IOAVServiceCreateWithService(ptr::null(), svc) };
            // A null handle means the service is there but not usable yet.
            found = Some(if av.is_null() {
                Err(Error::NotFound)
            } else {
                Ok((av, id))
            });
        }
        n += 1;
    });
    let (av, id) = found.unwrap_or(Err(Error::NotFound))?;
    Ok((av, id, n))
}

/// How long [`AvService::reconnect`] keeps looking for the panel after a
/// re-sync, as (attempts, delay) pairs. About 7 seconds in all.
const RECONNECT_BACKOFF: [(u32, u64); 2] = [(4, 250), (8, 750)];

/// One external display's AV service handle.
pub struct AvService {
    inner: CFTypeRef,
    /// Position among external services when opened.
    index: usize,
    /// Registry entry id, stable across enumeration order changes.
    entry: u64,
    /// External services present when opened.
    seen: usize,
}

// The CF handle is used only by the owning session; moving it between threads
// is fine, sharing it is not.
unsafe impl Send for AvService {}

impl AvService {
    /// Number of external displays exposing a DDC-capable AV service.
    pub fn count() -> usize {
        let mut n = 0;
        each_external(|_, _| n += 1);
        n
    }

    /// Open the external display at `index` (0 = first). Fails at once if it
    /// is not there.
    pub fn open(index: usize) -> Result<Self, Error> {
        let (inner, entry, seen) = open_once(Target::Index(index))?;
        Ok(AvService {
            inner,
            index,
            entry,
            seen,
        })
    }

    /// One reopen attempt: the same registry entry if it still exists, else
    /// the same index, but only once as many displays are back as before so
    /// a half-enumerated multi-monitor setup does not hand us the wrong one.
    fn reopen(&self) -> Result<(CFTypeRef, u64, usize), Error> {
        match open_once(Target::Entry(self.entry)) {
            Err(Error::NotFound) if Self::count() >= self.seen => {
                open_once(Target::Index(self.index))
            }
            r => r,
        }
    }
}

impl Drop for AvService {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { CFRelease(self.inner) };
        }
    }
}

impl I2c for AvService {
    fn write(&mut self, chip: u8, offset: u8, data: &[u8]) -> Result<(), Error> {
        let r = unsafe {
            IOAVServiceWriteI2C(
                self.inner,
                chip as u32,
                offset as u32,
                data.as_ptr().cast(),
                data.len() as u32,
            )
        };
        if r == KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::Io {
                op: "write",
                code: r,
            })
        }
    }

    fn read(&mut self, chip: u8, offset: u8, out: &mut [u8]) -> Result<(), Error> {
        let r = unsafe {
            IOAVServiceReadI2C(
                self.inner,
                chip as u32,
                offset as u32,
                out.as_mut_ptr().cast(),
                out.len() as u32,
            )
        };
        if r == KERN_SUCCESS {
            Ok(())
        } else {
            Err(Error::Io {
                op: "read",
                code: r,
            })
        }
    }

    /// Reopen after a re-sync. The service node vanishes from the registry for
    /// a second or two while the panel re-syncs, so this waits for it.
    fn reconnect(&mut self) -> Result<(), Error> {
        let mut last = Error::NotFound;
        for (tries, ms) in RECONNECT_BACKOFF {
            for _ in 0..tries {
                match self.reopen() {
                    Ok((fresh, entry, seen)) => {
                        if !self.inner.is_null() {
                            unsafe { CFRelease(self.inner) };
                        }
                        self.inner = fresh;
                        self.entry = entry;
                        self.seen = seen;
                        return Ok(());
                    }
                    Err(e) => last = e,
                }
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
        Err(last)
    }
}

/// Open the first external display as a ready-to-use DDC session, with the
/// panel profile picked from its EDID.
pub fn open_first() -> Result<crate::Ddc<AvService>, Error> {
    let mut d = crate::Ddc::new(AvService::open(0)?);
    d.detect_panel();
    Ok(d)
}
