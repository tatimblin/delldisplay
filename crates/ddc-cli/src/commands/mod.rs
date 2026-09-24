//! One module per command family.
//!
//! [`exec`] is the shared write path, output and exit codes; [`args`] holds the
//! shared flags and value parsers.

pub mod args;
pub mod basic;
pub mod exec;
pub mod identity;
pub mod kvm;
pub mod mapping;
pub mod picture;
pub mod pxp;
pub mod watch;

use ddc_transport::{Ddc, I2c};

use crate::Cmd;
use exec::Ctx;

/// Run a command that needs no display. `None` means open one and call [`run`].
pub fn offline(cmd: &Cmd, ctx: &Ctx) -> Option<u8> {
    match cmd {
        Cmd::List => Some(basic::list(ctx)),
        Cmd::Codes(a) => Some(basic::codes(a, ctx)),
        Cmd::Pxp(a) => pxp::offline(a, ctx),
        _ => None,
    }
}

pub fn run<T: I2c>(cmd: &Cmd, ctx: &Ctx, d: &mut Ddc<T>) -> u8 {
    match cmd {
        Cmd::List => basic::list(ctx),
        Cmd::Codes(a) => basic::codes(a, ctx),
        Cmd::Status => basic::status(d, ctx),
        Cmd::Get(a) => basic::get(d, a, ctx),
        Cmd::Caps => basic::caps(d, ctx),
        Cmd::Coverage => basic::coverage(d, ctx),
        Cmd::Set(a) => basic::set(d, a, ctx),
        Cmd::Identity(a) => identity::run(d, a, ctx),
        Cmd::Watch(a) => watch::run(d, a, ctx),
        Cmd::Picture(a) => picture::run(d, a, ctx),
        Cmd::Wake(w) => picture::wake(d, w, ctx),
        Cmd::Pxp(a) => pxp::run(d, a, ctx),
        Cmd::Kvm(a) => kvm::run(d, a, ctx),
        Cmd::Map(a) => mapping::map(d, a),
        Cmd::Pipmap(a) => mapping::pipmap(d, a),
    }
}
