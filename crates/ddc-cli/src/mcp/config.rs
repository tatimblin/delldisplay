//! `mcp.toml`: what the server may do on its own.
//!
//! The rules live here rather than in the agent's permission prompts, because
//! those prompts show up on the screen the user isn't looking at. A missing
//! file means the defaults; an unknown key is an error, so a typo can't
//! silently loosen anything.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::commands::args;

/// Where the config lives unless `--config` or `DELLDISPLAY_MCP_CONFIG` says
/// otherwise: `$XDG_CONFIG_HOME/delldisplay/mcp.toml`, else `~/.config/…`.
pub fn default_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("delldisplay/mcp.toml")
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
struct File {
    display: Option<usize>,
    self_input: Option<String>,
    allow_takeover: bool,
    cooldown_seconds: u64,
    notify: bool,
}

impl Default for File {
    fn default() -> Self {
        let c = Config::default();
        File {
            display: None,
            self_input: None,
            allow_takeover: c.allow_takeover,
            cooldown_seconds: c.cooldown_seconds,
            notify: c.notify,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Which display, counting from 0. `None` defers to `--display`.
    pub display: Option<usize>,
    /// This computer's input, when the panel doesn't report it.
    pub self_input: Option<u8>,
    /// Allow changes that take something off the screen.
    pub allow_takeover: bool,
    /// Minimum gap between two changes made through this server.
    pub cooldown_seconds: u64,
    /// Show the `reason` as a macOS notification after a change.
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            display: None,
            self_input: None,
            allow_takeover: false,
            cooldown_seconds: 10,
            notify: true,
        }
    }
}

impl Config {
    /// Read `path`. A file that doesn't exist is the defaults.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let f: File = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let self_input = match f.self_input.as_deref() {
            None | Some("auto") => None,
            Some(s) => Some(args::input(s).map_err(|e| format!("self_input: {e}"))?),
        };
        Ok(Config {
            display: f.display,
            self_input,
            allow_takeover: f.allow_takeover,
            cooldown_seconds: f.cooldown_seconds,
            notify: f.notify,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let missing = std::env::temp_dir().join("delldisplay-no-such-mcp.toml");
        assert_eq!(Config::load(&missing).unwrap(), Config::default());
    }

    #[test]
    fn every_key_parses() {
        let c = Config::parse(
            "display = 1\nself_input = \"dp2\"\nallow_takeover = true\n\
             cooldown_seconds = 0\nnotify = false\n",
        )
        .unwrap();
        assert_eq!(
            c,
            Config {
                display: Some(1),
                self_input: Some(0x13),
                allow_takeover: true,
                cooldown_seconds: 0,
                notify: false,
            }
        );
        assert_eq!(
            Config::parse("self_input = \"auto\"").unwrap().self_input,
            None
        );
    }

    #[test]
    fn typos_and_bad_inputs_are_errors() {
        assert!(Config::parse("allow_takover = true").is_err());
        assert!(Config::parse("self_input = \"vga\"")
            .unwrap_err()
            .contains("self_input"));
        assert!(Config::parse("cooldown_seconds = -1").is_err());
    }
}
