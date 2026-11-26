// SPDX-License-Identifier: MPL-2.0

//! The viewers' sessions: the HTTPS server and its signalling, each viewer's WebRTC peer
//! connection, and what goes between it and the guest (keyboard and pointer, screens, sound).

pub mod audio;
pub mod http;
pub mod input;
pub mod layout;
pub mod lock;
pub mod peer;
pub mod pointer;
pub mod shared;
pub mod signalling;
pub mod transport;
pub mod viewer;

pub use http::{Tls, load_tls, serve};
pub use shared::{Request, Screen, Shared};
pub use signalling::{Admission, Socket, open};
