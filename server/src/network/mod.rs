// SPDX-License-Identifier: MPL-2.0

//! How clients outside the host's network reach its sessions: STUN for the address a NAT maps,
//! one UDP port for every session when asked, TURN relays where no direct path gets through.

pub mod mux;
pub mod stun;
pub mod turn;
