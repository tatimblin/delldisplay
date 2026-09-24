//! What the server remembers between tool calls and restarts, per monitor.
//!
//! Everything lives under `<state dir>/mcp/`:
//!
//! - `<monitor>.json`: the view to put back, the last write time and the last
//!   input this computer was seen on. Written whole to a temp file and renamed.
//! - `lock`: held for a whole tool call, so two servers on one Mac (two agent
//!   sessions) take turns instead of interleaving I2C frames.
//! - `log.jsonl`: one line per change, refusal or failure.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ddc_core::arrange::View;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The on-disk form of a [`View`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rec {
    pub layout: u8,
    pub main: u8,
    pub sub: u16,
}

impl From<View> for Rec {
    fn from(v: View) -> Rec {
        Rec {
            layout: v.layout,
            main: v.main,
            sub: v.sub,
        }
    }
}

impl From<Rec> for View {
    fn from(r: Rec) -> View {
        View {
            layout: r.layout,
            main: r.main,
            sub: r.sub,
        }
    }
}

/// A change this computer made that `display_restore` can take back.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Saved {
    /// What the monitor showed before the first change in the chain.
    pub before: Rec,
    /// What it read back after the latest one.
    pub after: Rec,
    pub reason: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(default)]
pub struct State {
    pub v: u32,
    /// The input this computer was last detected on.
    pub this_input: Option<u8>,
    /// Unix seconds of the last change made through the server.
    pub last_write: Option<u64>,
    pub restore: Option<Saved>,
    /// The capability string, which doesn't change for a given monitor.
    pub caps: Option<String>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `base` is the CLI's state directory.
    pub fn new(base: &Path) -> Store {
        Store {
            dir: base.join("mcp"),
        }
    }

    /// [`lock`](Store::lock), giving up after `wait`: `Ok(None)` means
    /// another call still holds it.
    pub fn lock_within(&self, wait: Duration) -> Result<Option<File>, String> {
        let f = self.lock_file()?;
        let start = Instant::now();
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(Some(f)),
                Err(TryLockError::WouldBlock) if start.elapsed() < wait => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(TryLockError::WouldBlock) => return Ok(None),
                Err(TryLockError::Error(e)) => return Err(e.to_string()),
            }
        }
    }

    fn lock_file(&self) -> Result<File, String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let path = self.dir.join("lock");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Hold this for the whole call. Released when dropped.
    #[cfg(test)]
    pub fn lock(&self) -> Result<File, String> {
        let f = self.lock_file()?;
        f.lock().map_err(|e| e.to_string())?;
        Ok(f)
    }

    fn path(&self, monitor: &str) -> PathBuf {
        let safe: String = monitor
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        self.dir.join(format!("{safe}.json"))
    }

    /// A missing or unreadable file is a fresh state.
    pub fn load(&self, monitor: &str) -> State {
        fs::read_to_string(self.path(monitor))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, monitor: &str, state: &State) -> Result<(), String> {
        let path = self.path(monitor);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&State {
            v: 1,
            ..state.clone()
        })
        .expect("state is plain data");
        fs::create_dir_all(&self.dir)
            .and_then(|_| fs::write(&tmp, text))
            .and_then(|_| fs::rename(&tmp, &path))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Append one line to the log. Best effort: a full disk shouldn't fail a
    /// change that already happened.
    pub fn log(&self, entry: &Value) {
        let _ = fs::create_dir_all(&self.dir).and_then(|_| {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join("log.jsonl"))?;
            writeln!(f, "{entry}")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Store {
        let base = crate::test_support::state_dir(&format!("store-{name}"));
        let _ = fs::remove_dir_all(&base);
        Store::new(&base)
    }

    #[test]
    fn state_round_trips_and_starts_empty() {
        let s = store("round-trip");
        assert_eq!(s.load("DELL U4323QE 123"), State::default());
        let state = State {
            v: 1,
            this_input: Some(0x13),
            last_write: Some(10),
            restore: Some(Saved {
                before: Rec {
                    layout: 0,
                    main: 0x0F,
                    sub: 0,
                },
                after: Rec {
                    layout: 0x24,
                    main: 0x0F,
                    sub: 0x13,
                },
                reason: String::from("tests"),
                at: 10,
            }),
            caps: Some(String::from("(vcp(10 60))")),
        };
        s.save("DELL U4323QE 123", &state).unwrap();
        assert_eq!(s.load("DELL U4323QE 123"), state);
        assert_eq!(s.load("another monitor"), State::default());
    }

    #[test]
    fn the_lock_is_exclusive_across_handles() {
        let s = store("lock");
        let held = s.lock().unwrap();
        let path = s.dir.join("lock");
        let other = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(other.try_lock().is_err());
        assert!(s.lock_within(Duration::from_millis(100)).unwrap().is_none());
        drop(held);
        assert!(other.try_lock().is_ok());
    }
}
