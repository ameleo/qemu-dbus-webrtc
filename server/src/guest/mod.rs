// SPDX-License-Identifier: MPL-2.0

//! What the guest agent brings: the clipboard, and the client's drives, printers and cameras in
//! the guest; and the consent of the guest's user before someone else watches.

pub mod agent;
pub mod cameras;
pub mod clipboard;
pub mod consent;
pub mod drives;
pub mod printers;
