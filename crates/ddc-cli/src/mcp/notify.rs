//! Say why the screen just changed, as a macOS notification.

/// Post `message` and return whether the notification was handed off. Never
/// waits on it: `osascript` is reaped on a thread of its own.
#[cfg(target_os = "macos")]
pub fn post(title: &str, message: &str) -> bool {
    use std::process::{Command, Stdio};
    // The text goes in as arguments, never into the script, so nothing an
    // agent writes can become AppleScript.
    let child = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            "--",
            title,
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut c) => {
            std::thread::spawn(move || c.wait());
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn post(_: &str, _: &str) -> bool {
    false
}
