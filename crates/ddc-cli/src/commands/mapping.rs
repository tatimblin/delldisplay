//! `map` and `pipmap`: step through a code's values while someone watches the
//! screen and types what each one did.
//!
//! Writes the panel over and over, so it's for supervised use only. The code,
//! layout and input are put back at the end, and the notes go to
//! `map-0xNN.md` in the current directory.

use std::io::{BufRead, Write as _};
use std::time::Duration;

use clap::Args;
use ddc_core::vcp::Vcp;
use ddc_transport::{Ddc, I2c};

use super::args;
use super::exec::exit;

#[derive(Args, Debug)]
#[command(after_help = "Writes its notes to map-0xNN.md in the current directory.")]
pub struct Map {
    /// The code to map: a name from `codes`, or 0xNN.
    #[arg(value_parser = args::code)]
    pub code: u8,
    /// Switch to this PiP/PBP layout first, for codes that only matter there.
    #[arg(long, value_parser = args::layout, value_name = "LAYOUT")]
    pub with_pip: Option<u8>,
    #[command(flatten)]
    pub walk: Walk,
}

#[derive(Args, Debug)]
#[command(
    after_help = "Turns PiP off between layouts. Writes its notes to map-0xE9.md in the \
                        current directory."
)]
pub struct Pipmap {
    #[command(flatten)]
    pub walk: Walk,
}

#[derive(Args, Debug)]
pub struct Walk {
    /// Values to try, comma-separated: decimal, or 0x for hex. Defaults to the
    /// values the panel advertises.
    #[arg(long, value_delimiter = ',', value_parser = args::byte, value_name = "LIST")]
    pub values: Option<Vec<u8>>,
    /// Seconds to leave each value on screen.
    #[arg(long, value_name = "SECS", default_value_t = 6)]
    pub dwell: u64,
}

pub fn map<T: I2c>(d: &mut Ddc<T>, a: &Map) -> u8 {
    walk(d, a.code, &a.walk, a.with_pip, false)
}

pub fn pipmap<T: I2c>(d: &mut Ddc<T>, a: &Pipmap) -> u8 {
    walk(d, Vcp::PIP_MODE, &a.walk, None, true)
}

fn byte<T: I2c>(d: &mut Ddc<T>, code: u8) -> Option<u8> {
    d.get(code).ok().map(|r| r.current as u8)
}

fn pause(secs: u64) {
    std::thread::sleep(Duration::from_secs(secs));
}

/// `reset_between` writes 0 after each value; for 0xE9 that's PiP off, which
/// keeps one layout from bleeding into the next.
fn walk<T: I2c>(
    d: &mut Ddc<T>,
    code: u8,
    w: &Walk,
    with_pip: Option<u8>,
    reset_between: bool,
) -> u8 {
    let (was, was_pip, was_input) = (
        byte(d, code),
        byte(d, Vcp::PIP_MODE),
        byte(d, Vcp::INPUT_SOURCE),
    );
    let name = d.panel().lookup(code).map_or("?", |v| v.name);
    let values = match &w.values {
        Some(v) => v.clone(),
        None => d
            .capabilities()
            .ok()
            .and_then(|c| c.legal_values(code).map(<[u8]>::to_vec))
            .unwrap_or_default(),
    };
    if values.is_empty() {
        eprintln!("no values known for 0x{code:02X}; pass --values");
        return exit::USAGE;
    }
    if let Some(layout) = with_pip {
        println!("switching to layout 0x{layout:02X} first");
        let _ = d.set(Vcp::PIP_MODE, layout.into());
        pause(6);
    }
    println!(
        "mapping 0x{code:02X} ({name}): {} values, {}s each",
        values.len(),
        w.dwell
    );
    println!("type what you saw and press Enter; blank skips");
    println!("if it goes wrong: delldisplay set pip off && delldisplay set input usb-c\n");

    let mut notes: Vec<(u8, String)> = Vec::new();
    let mut lines = std::io::stdin().lock().lines();
    for (i, v) in values.iter().enumerate() {
        print!("[{}/{}] 0x{v:02X} ... ", i + 1, values.len());
        let _ = std::io::stdout().flush();
        if d.set(code, (*v).into()).is_err() {
            println!("write failed");
            notes.push((*v, String::from("write failed")));
            continue;
        }
        pause(w.dwell);
        // The panel may store a different value than was written.
        let alias = match byte(d, code) {
            Some(r) if r != *v => format!(" [reads back 0x{r:02X}]"),
            _ => String::new(),
        };
        if reset_between {
            let _ = d.set(code, 0);
            pause(3);
        }
        // Some layouts move the main input; put it back before asking.
        if let (Some(want), Some(now)) = (was_input, byte(d, Vcp::INPUT_SOURCE)) {
            if now != want {
                print!("(input moved to 0x{now:02X}, restoring) ");
                let _ = d.set(Vcp::INPUT_SOURCE, want.into());
                pause(5);
            }
        }
        print!("{alias} what did you see? > ");
        let _ = std::io::stdout().flush();
        let Some(Ok(answer)) = lines.next() else {
            println!("\nno terminal on stdin, stopping");
            break;
        };
        let answer = answer.trim();
        let seen = if answer.is_empty() {
            "(skipped)"
        } else {
            answer
        };
        notes.push((*v, format!("{seen}{alias}")));
    }

    println!("\nputting things back");
    for (c, v, secs) in [
        (code, was, 2),
        (Vcp::PIP_MODE, was_pip, 4),
        (Vcp::INPUT_SOURCE, was_input, 0),
    ] {
        if let Some(v) = v.filter(|v| byte(d, c) != Some(*v)) {
            let _ = d.set(c, v.into());
            pause(secs);
        }
    }

    if notes.is_empty() {
        return exit::OK;
    }
    let mut table = String::from("| value | seen |\n|-------|------|\n");
    for (v, seen) in &notes {
        table.push_str(&format!("| 0x{v:02X} | {seen} |\n"));
    }
    println!("\n{table}");
    let file = format!("map-0x{code:02X}.md");
    match std::fs::write(&file, format!("# VCP 0x{code:02X} ({name})\n\n{table}")) {
        Ok(()) => println!("wrote {file}"),
        Err(e) => eprintln!("couldn't write {file}: {e}"),
    }
    exit::OK
}
