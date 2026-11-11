// SPDX-License-Identifier: MPL-2.0

//! Attachment to a running QEMU virtual machine: QMP, and the D-Bus display (`-display dbus`).

pub mod agent;
pub mod display;
pub mod input;
mod qmp;
