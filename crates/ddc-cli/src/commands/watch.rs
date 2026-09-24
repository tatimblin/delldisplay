//! `watch`: notice changes made with the monitor's own buttons.
//!
//! Reads a few codes every interval and prints what changed. MCCS's change
//! flags (0x02/0x52) never fire on the U4323QE, so this diffs instead. The
//! input (0x60) is sampled when it looks changed, since Auto Select can be
//! scanning through inputs.

use std::collections::BTreeMap;
use std::time::Duration;

use clap::Args as ClapArgs;
use ddc_core::vcp::{Panel, Vcp};
use ddc_transport::{Ddc, I2c};
use serde::Serialize;

use super::basic::{is_level, masked, shown};
use super::exec::{self, exit, print_json, Ctx};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Milliseconds between polls.
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    pub interval: u64,
    /// Stop after N polls. Without it, run until ctrl-c.
    #[arg(long, value_name = "N")]
    pub polls: Option<u64>,
    /// Same as `--polls 1`.
    #[arg(long, conflicts_with = "polls")]
    pub once: bool,
}

/// Codes read each poll: the ones people change from the OSD.
const POLLED: &[u8] = &[0x10, 0x12, 0x14, 0x60, 0x62, 0xD6, 0xE9, 0xE8];

/// Give up after this many polls in a row where nothing answered.
const GIVE_UP: u32 = 5;

/// One line of `--json` output.
#[derive(Serialize)]
struct Event {
    event: &'static str,
    code: u8,
    hex: String,
    name: &'static str,
    from: u16,
    to: u16,
}

pub fn run<T: I2c>(d: &mut Ddc<T>, a: &Args, ctx: &Ctx) -> u8 {
    let polls = if a.once { Some(1) } else { a.polls };
    watch(
        d,
        Duration::from_millis(a.interval),
        polls,
        &[Vcp::INPUT_SOURCE],
        ctx,
    )
}

/// Every code in [`POLLED`] that answered, masked.
///
/// Codes in `sample` get [`Ddc::sample_mode`] (nine reads, about 2 s) only
/// when there's nothing to compare with or a single read disagrees with
/// `last`, so a quiet poll costs one read per code.
fn read_all<T: I2c>(
    d: &mut Ddc<T>,
    sample: &[u8],
    last: Option<&BTreeMap<u8, u16>>,
) -> BTreeMap<u8, u16> {
    let panel = d.panel();
    POLLED
        .iter()
        .filter_map(|&c| {
            let before = last.and_then(|l| l.get(&c));
            let r = match d.get(c) {
                Ok(r) if !sample.contains(&c) => Ok(r),
                Ok(r) if before == Some(&masked(panel, c, &r)) => Ok(r),
                _ if sample.contains(&c) => d.sample_mode(c),
                r => r,
            };
            r.ok().map(|r| (c, masked(panel, c, &r)))
        })
        .collect()
}

fn watch<T: I2c>(
    d: &mut Ddc<T>,
    interval: Duration,
    polls: Option<u64>,
    sample: &[u8],
    ctx: &Ctx,
) -> u8 {
    let mut last = read_all(d, sample, None);
    let mut dead = u32::from(last.is_empty());
    if !ctx.json {
        println!(
            "watching {} codes, {}ms between polls; change something at the monitor",
            POLLED.len(),
            interval.as_millis()
        );
    }
    let mut n = 0;
    while polls.is_none_or(|max| n < max) {
        std::thread::sleep(ctx.wait(interval));
        n += 1;
        let now = read_all(d, sample, Some(&last));
        dead = if now.is_empty() { dead + 1 } else { 0 };
        if dead >= GIVE_UP {
            return exec::fail(
                ctx,
                &format!("the panel stopped answering ({GIVE_UP} polls in a row)"),
            );
        }
        for (&code, &to) in &now {
            match last.insert(code, to) {
                Some(from) if from != to => report(d.panel(), code, from, to, ctx),
                _ => {}
            }
        }
    }
    exit::OK
}

fn report(panel: &Panel, code: u8, from: u16, to: u16, ctx: &Ctx) {
    let name = panel.lookup(code).map_or("", |c| c.name);
    if ctx.json {
        print_json(&Event {
            event: "change",
            code,
            hex: format!("0x{code:02X}"),
            name,
            from,
            to,
        });
        return;
    }
    let label = |v: u16| match is_level(code) {
        true => v.to_string(),
        false => shown(v, panel.value_name(code, v as u8)),
    };
    println!("0x{code:02X} {name} {} -> {}", label(from), label(to));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run;
    use ddc_transport::testing::{fast, u4323qe, DeadI2c, ReplayI2c};

    fn ctx() -> Ctx {
        Ctx {
            json: true,
            display: 0,
            state_dir: None,
            max_dwell: Some(Duration::ZERO),
        }
    }

    #[test]
    fn polls_n_times_then_stops() {
        let mut d = fast(u4323qe().on_get(0x10, 54));
        assert_eq!(
            watch(&mut d, Duration::ZERO, Some(3), &[], &ctx()),
            exit::OK
        );
        let reads = d
            .transport
            .raw_ops()
            .iter()
            .filter(|o| **o == ddc_transport::testing::Op::Get(0x10))
            .count();
        assert_eq!(
            reads,
            4 * 2,
            "the first read plus three polls, each frame doubled"
        );
        assert!(d.transport.sets().is_empty(), "watching never writes");
    }

    /// Reads of 0x60, counting repeats but not the double write.
    fn input_reads(t: &ReplayI2c) -> usize {
        let get = ddc_transport::testing::Op::Get(0x60);
        t.raw_ops().iter().filter(|o| **o == get).count() / 2
    }

    #[test]
    fn a_quiet_poll_reads_the_input_once() {
        let mut d = fast(u4323qe());
        assert_eq!(
            watch(&mut d, Duration::ZERO, Some(3), &[0x60], &ctx()),
            exit::OK
        );
        let reads = input_reads(&d.transport);
        // one read then the nine-read sample to start, then one per poll
        assert_eq!(reads, 1 + 9 + 3);
    }

    #[test]
    fn an_input_that_looks_changed_is_sampled() {
        let mut d = fast(u4323qe());
        let mut last = read_all(&mut d, &[0x60], None);
        last.insert(0x60, 0x0F);
        d.transport.clear_writes();
        let now = read_all(&mut d, &[0x60], Some(&last));
        assert_eq!(now[&0x60], 0x1B);
        let reads = input_reads(&d.transport);
        assert_eq!(reads, 1 + 9);
    }

    #[test]
    fn a_dead_panel_gives_up_with_exit_1() {
        let mut d = fast(DeadI2c);
        assert_eq!(
            watch(&mut d, Duration::ZERO, None, &[], &ctx()),
            exit::FAILED
        );
    }

    #[test]
    fn a_panel_with_some_codes_missing_keeps_going() {
        let mut d = fast(ReplayI2c::panel().on_get(0x10, 54));
        assert_eq!(
            watch(&mut d, Duration::ZERO, Some(6), &[], &ctx()),
            exit::OK
        );
    }

    #[test]
    fn the_event_shape_is_flat_and_masked() {
        let e = Event {
            event: "change",
            code: 0x60,
            hex: "0x60".into(),
            name: "input",
            from: 0x1B,
            to: 0x0F,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "event": "change", "code": 96, "hex": "0x60", "name": "input", "from": 27, "to": 15 })
        );
    }

    #[test]
    fn once_is_one_poll() {
        let mut d = fast(u4323qe());
        assert_eq!(run(&mut d, "watch --once --interval 0 --json"), exit::OK);
    }
}
