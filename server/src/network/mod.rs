// SPDX-License-Identifier: MPL-2.0

//! How clients outside the host's network reach its sessions: STUN for the address a NAT maps,
//! TURN relays where no direct path gets through.

pub mod stun;
pub mod turn;
