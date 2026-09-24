//! `delldisplay`: control a Dell monitor over DDC/CI from the shell.
//!
//! Parse with clap, open the display only if the command needs one, then hand
//! off to [`commands`]. Help and usage errors are settled before anything is
//! opened, so they never touch the device.

mod commands;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ddc_transport::{Ddc, I2c};

use commands::args::Write;
use commands::exec::{self, exit, Ctx};
use commands::{basic, identity, kvm, mapping, picture, pxp, watch};

const HELP: &str = "\
{about}

{usage-heading} {usage}

Reading:
  list      List external displays
  status    Read the common features
  get       Read one feature
  caps      Read and parse the capabilities string
  codes     List the known VCP codes
  coverage  Show how much of what the panel advertises is understood
  identity  EDID, firmware, and VCP state export/import
  watch     Print changes made with the monitor's own buttons

Writing:
  set       Write one feature
  picture   Colour preset, picture mode, volume, mute, power, restores
  wake      Power on and clear PowerNap, retried
  pxp       PiP/PBP layout, sub-window sources, window actions
  kvm       USB KVM: association map, commit, switch

Exploring (writes the panel over and over, so stay at the screen):
  map       Step through a code's values and note what each one does
  pipmap    Same, for the PiP/PBP layouts

Options:
{options}{after-help}";

const GLOBAL: &str = "Global options";

const AFTER_HELP: &str = "\
Exit codes:
  0  done
  1  the panel or transport failed
  2  bad usage
  3  refused, nothing was written

`delldisplay <command> --help` lists a command's verbs and flags.";

/// Control a Dell monitor over DDC/CI.
#[derive(Parser, Debug)]
#[command(
    name = "delldisplay",
    version,
    help_template = HELP,
    after_help = AFTER_HELP,
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Print JSON on stdout, errors included.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub json: bool,
    /// Which external display, counting from 0 (see `list`).
    #[arg(long, global = true, value_name = "N", default_value_t = 0, help_heading = GLOBAL)]
    pub display: usize,
    /// Send each request frame once instead of twice.
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub no_double_write: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List external displays.
    List,
    /// Read the common features.
    Status,
    /// Read one feature.
    Get(basic::Get),
    /// Read and parse the capabilities string.
    Caps,
    /// List the known VCP codes. Needs no display.
    Codes(basic::Codes),
    /// Show how much of what the panel advertises is understood.
    Coverage,
    /// EDID, firmware, and VCP state export/import.
    Identity(identity::Args),
    /// Poll a few codes and print changes made with the monitor's own buttons.
    Watch(watch::Args),
    /// Write one feature.
    Set(basic::Set),
    /// Colour preset, picture mode, volume, mute, power, restores.
    Picture(picture::Args),
    /// Power on and clear PowerNap, retried. Same as `picture wake`.
    Wake(Write),
    /// PiP/PBP layout, sub-window sources, window actions.
    Pxp(pxp::Args),
    /// USB KVM: association map, commit, switch.
    Kvm(kvm::Args),
    /// Step through an enumerated code's values and note what each one does.
    Map(mapping::Map),
    /// Step through the PiP/PBP layouts (0xE9) and note what each one does.
    Pipmap(mapping::Pipmap),
}

/// Knobs tests turn: where the mute memo lives, and a cap on every wait.
#[derive(Default)]
struct Env {
    state_dir: Option<PathBuf>,
    max_dwell: Option<Duration>,
}

/// Parse `args`, then run the command. `open` is only called for commands
/// that need the display.
fn entry<'d, T: I2c + 'd>(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    env: Env,
    open: impl FnOnce(&Cli) -> Result<&'d mut Ddc<T>, String>,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                exit::USAGE
            } else {
                exit::OK
            };
        }
    };
    let ctx = Ctx {
        json: cli.json,
        display: cli.display,
        state_dir: env.state_dir,
        max_dwell: env.max_dwell,
    };
    if let Some(code) = commands::offline(&cli.cmd, &ctx) {
        return code;
    }
    match open(&cli) {
        Ok(d) => commands::run(&cli.cmd, &ctx, d),
        Err(e) => exec::fail(&ctx, &e),
    }
}

fn main() -> ExitCode {
    let mut slot = None;
    let code = entry(std::env::args_os(), Env::default(), |cli| {
        Ok(slot.insert(open(cli)?))
    });
    ExitCode::from(code)
}

/// How long to keep looking for a display that's mid-resync.
#[cfg(target_os = "macos")]
const FIND_FOR: Duration = Duration::from_secs(3);

#[cfg(target_os = "macos")]
fn open(cli: &Cli) -> Result<Ddc<ddc_transport::macos::AvService>, String> {
    use ddc_transport::{macos::AvService, Error, Policy};
    let start = std::time::Instant::now();
    let av = loop {
        match AvService::open(cli.display) {
            Err(Error::NotFound) if start.elapsed() < FIND_FOR => {
                std::thread::sleep(Duration::from_millis(250))
            }
            r => break r.map_err(|e| e.to_string())?,
        }
    };
    let policy = Policy {
        double_write: !cli.no_double_write,
        ..Policy::default()
    };
    let mut d = Ddc::with_policy(av, policy);
    // Unknown models keep the U4323QE default, with a note, rather than
    // guessing at a relative.
    if let Some(model) = d.detect_panel() {
        if !d.panel().matches(&model) {
            eprintln!(
                "note: no profile for {model}, so the U4323QE's is used. Value names and \
                 quirks may not match this panel."
            );
        }
    }
    Ok(d)
}

#[cfg(not(target_os = "macos"))]
fn open(_: &Cli) -> Result<Ddc<NoTransport>, String> {
    Err(String::from(
        "no DDC transport for this OS yet; only macOS is supported",
    ))
}

/// Stands in for a transport on platforms that don't have one.
#[cfg(not(target_os = "macos"))]
enum NoTransport {}

#[cfg(not(target_os = "macos"))]
impl I2c for NoTransport {
    fn write(&mut self, _: u8, _: u8, _: &[u8]) -> Result<(), ddc_transport::Error> {
        match *self {}
    }
    fn read(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), ddc_transport::Error> {
        match *self {}
    }
}

/// Run a command line against a fake panel. Every wait is zero.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn run<T: I2c>(d: &mut Ddc<T>, line: &str) -> u8 {
        run_in(d, line, "default")
    }

    /// Same, with the mute memo kept under a test-specific directory.
    pub fn run_in<T: I2c>(d: &mut Ddc<T>, line: &str, state: &str) -> u8 {
        let args = std::iter::once("delldisplay").chain(line.split_whitespace());
        let env = Env {
            state_dir: Some(state_dir(state)),
            max_dwell: Some(Duration::ZERO),
        };
        entry(args, env, |_| Ok(d))
    }

    pub fn state_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("delldisplay-test-{}-{name}", std::process::id()))
    }

    /// Parse and run without a display; opening one fails the test.
    pub fn run_offline(line: &str) -> u8 {
        let args = std::iter::once("delldisplay").chain(line.split_whitespace());
        entry::<ddc_transport::testing::DeadI2c>(args, Env::default(), |_| {
            panic!("`{line}` opened the display")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::run_offline;
    use super::*;
    use clap::CommandFactory;

    /// Every command path, e.g. `["pxp", "swap"]`.
    fn paths() -> Vec<Vec<String>> {
        fn walk(cmd: &clap::Command, prefix: Vec<String>, out: &mut Vec<Vec<String>>) {
            for sub in cmd.get_subcommands() {
                let mut p = prefix.clone();
                p.push(sub.get_name().to_string());
                out.push(p.clone());
                walk(sub, p, out);
            }
        }
        let mut out = Vec::new();
        walk(&Cli::command(), Vec::new(), &mut out);
        out
    }

    #[test]
    fn help_for_every_command_never_opens_the_display() {
        assert_eq!(run_offline("--help"), exit::OK);
        let all = paths();
        assert!(all.len() > 40, "only {} command paths", all.len());
        for p in all {
            assert_eq!(run_offline(&format!("{} --help", p.join(" "))), exit::OK);
            assert_eq!(run_offline(&format!("{} -h", p.join(" "))), exit::OK);
        }
    }

    #[test]
    fn top_level_help_lists_every_command() {
        let help = Cli::command().render_help().to_string();
        for sub in Cli::command().get_subcommands() {
            let name = sub.get_name();
            if name != "help" {
                assert!(
                    help.contains(&format!("\n  {name} ")),
                    "{name} missing:\n{help}"
                );
            }
        }
    }

    #[test]
    fn unknown_flags_are_usage_errors_before_anything_is_opened() {
        for line in [
            "picture volume 20 --dryrun",
            "picture restore factory --i-mean-it --dryrun",
            "kvm switch --dryrun",
            "kvm commit dp2 --dwell 0",
            "pxp swap --dryrun",
            "pxp apply quad --sub4 dp",
            "identity import snap.json --dryrun",
            "watch --flags",
            "set brightness 50 --dryrun",
        ] {
            assert_eq!(run_offline(line), exit::USAGE, "{line}");
        }
    }

    #[test]
    fn bad_values_are_usage_errors_too() {
        for line in [
            "--display abc status",
            "status --display",
            "get nosuchcode",
            "set 0x10",
            "pxp sub main dp2",
            "pxp sub sub1 vga",
            "pxp geometry nonsense",
            "kvm associate nosuchinput 1",
            "picture preset nosuchpreset",
            "picture volume loud",
            "picture restore nuke --i-mean-it",
            "identity import snap.json --only 0x10,zz",
            "map 0x60 --values 0x1B,dp",
            "nosuchcommand",
            "",
        ] {
            assert_eq!(run_offline(line), exit::USAGE, "{line}");
        }
    }

    #[test]
    fn flags_can_go_anywhere() {
        let cli = Cli::try_parse_from(["delldisplay", "set", "0x04", "--i-mean-it", "1"]).unwrap();
        let Cmd::Set(s) = cli.cmd else { panic!() };
        assert!(s.i_mean_it);
        assert_eq!((s.code, s.value.as_str()), (0x04, "1"));

        let cli =
            Cli::try_parse_from(["delldisplay", "status", "--json", "--display", "1"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.display, 1);
    }

    #[test]
    fn commands_that_need_no_display_run_without_one() {
        for line in [
            "codes",
            "codes --json",
            "pxp layouts",
            "pxp geometry quad --json",
        ] {
            assert_eq!(run_offline(line), exit::OK, "{line}");
        }
        assert_eq!(run_offline("codes --model nosuch"), exit::USAGE);
    }
}
