// SPDX-License-Identifier: MPL-2.0

//! The messages between qemu-dbus-webrtc and its clients, in Protocol Buffers.
//!
//! `proto/qdw.proto` is the reference: clients in other languages generate their code from it.
//! This crate holds the Rust types generated from it, with [`prost::Message`] to encode and
//! decode them.

pub use prost::Message;

/// Version 1 of the protocol (`package qdw.v1`).
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/qdw.v1.rs"));
}

/// The path of the signalling WebSocket on the server.
pub const SIGNALLING_PATH: &str = "/ws";

/// The data channel of the client's keyboard and pointer: [`v1::Input`] messages.
pub const INPUT_CHANNEL: &str = "input";

/// The data channel of the guest pointer's shape: [`v1::PointerUpdate`] messages.
pub const POINTER_CHANNEL: &str = "pointer";

/// The data channel of the clipboard, both ways: [`v1::ClipboardMessage`] messages.
pub const CLIPBOARD_CHANNEL: &str = "clipboard";

/// The data channel of what the client wants of its session: [`v1::ControlMessage`] messages.
pub const CONTROL_CHANNEL: &str = "control";

/// The largest data chunk of a [`v1::ClipboardData`] or [`v1::ClipboardFileData`]: 60 KiB.
/// A data channel message is 64 KiB at most, whole, when the other side announces no larger
/// limit in its SDP (RFC 8841, and the server announces none); libwebrtc does not send a
/// larger one. The 4 KiB left are for the message's other fields.
pub const MAX_CHUNK: usize = 60 * 1024;

/// The clipboard type of files and directories: its data is a [`v1::ClipboardFileList`].
pub const FILE_LIST_MIME: &str = "application/vnd.qdw.file-list";

/// The largest piece of a file a [`v1::ClipboardFileRequest`] asks for: 4 MiB.
pub const MAX_FILE_PIECE: u32 = 4 * 1024 * 1024;

/// The largest clipboard content either side sends: 32 MiB.
pub const MAX_CLIPBOARD: usize = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::v1::{
        ClientMessage, Hello, IceCandidate, ServerMessage, client_message, server_message,
    };
    use super::*;

    /// The bytes on the wire are what other clients decode: keep them stable.
    #[test]
    fn wire_format() {
        let hello = ClientMessage {
            message: Some(client_message::Message::Hello(Hello {
                codecs: vec!["video/AV1".into()],
            })),
        };
        assert_eq!(hello.encode_to_vec(), b"\x0a\x0b\x0a\x09video/AV1");

        let ice = ServerMessage {
            message: Some(server_message::Message::Ice(IceCandidate {
                candidate: "c".into(),
                sdp_mline_index: 1,
            })),
        };
        let bytes = ice.encode_to_vec();
        assert_eq!(bytes, b"\x12\x05\x0a\x01c\x10\x01");
        assert_eq!(ServerMessage::decode(bytes.as_slice()).unwrap(), ice);
    }
}
