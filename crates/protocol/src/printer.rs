// SPDX-License-Identifier: MPL-2.0

//! Printer messages over a data channel: those larger than [`crate::MAX_CHUNK`] go in fragments.

use crate::v1::PrinterMessage;

pub use crate::fragment::{ReassembleError, encode};

/// Puts fragmented printer messages back together.
pub type Reassembler = crate::fragment::Reassembler<PrinterMessage>;
