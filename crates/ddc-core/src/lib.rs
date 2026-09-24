//! Platform-independent DDC/CI protocol plus Dell panel logic (guards, plans,
//! KVM/PxP/picture state). No OS calls.
//!
//! Pair it with an `I2c` transport from `ddc-transport` (or your own) to talk
//! to a real display.
#![forbid(unsafe_code)]

pub mod arrange;
pub mod bits;
pub mod caps;
pub mod edid;
#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
pub mod frame;
pub mod guard;
pub mod identity;
pub mod kvm;
pub mod picture;
pub mod plan;
pub mod power;
pub mod pxp;
pub mod state;
pub mod vcp;

pub use bits::{features, input_word, panes, status, Arrival, Features, InputWord, Panes, Status};
pub use caps::{Capabilities, VcpEntry};
pub use edid::{Edid, EdidError};
pub use frame::{
    decode_caps_fragment, decode_get_reply, encode_caps, encode_get, encode_set, DecodeError,
    Reply, ValueKind, DDC_CHIP, DDC_SRC, EDID_CHIP,
};
pub use guard::{Intent, Snapshot, Verdict};
pub use plan::{Failure, Outcome, Plan, Step};
pub use vcp::{CodeInfo, Panel, Provenance, Vcp};
