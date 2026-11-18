// SPDX-License-Identifier: MPL-2.0

//! The console page: the server serves it to its viewers, and a connection broker built on
//! qemu-dbus-webrtc serves this same page, from the version it depends on, rather than one a host
//! sends it.

/// The page, HTML with its scripts and styles inline.
pub const PAGE: &str = include_str!("../index.html");
