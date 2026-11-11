// SPDX-License-Identifier: MPL-2.0

//! Drive messages over a data channel: those larger than [`crate::MAX_CHUNK`] go in fragments.

use crate::v1::DriveMessage;

pub use crate::fragment::{ReassembleError, encode};

/// Puts fragmented drive messages back together.
pub type Reassembler = crate::fragment::Reassembler<DriveMessage>;
