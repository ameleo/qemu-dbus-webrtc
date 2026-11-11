// SPDX-License-Identifier: MPL-2.0

//! Drive messages over a data channel: those larger than [`MAX_CHUNK`] go in fragments.

use prost::Message;

use crate::MAX_CHUNK;
use crate::v1::{DriveFragment, DriveMessage, drive_message};

/// The data channel messages that carry `message`: itself when it fits, otherwise its encoded
/// bytes in [`DriveFragment`]s, to send one after the other with nothing in between.
pub fn encode(message: &DriveMessage) -> Vec<Vec<u8>> {
    let bytes = message.encode_to_vec();
    if bytes.len() <= MAX_CHUNK {
        return vec![bytes];
    }
    let count = bytes.len().div_ceil(MAX_CHUNK);
    bytes
        .chunks(MAX_CHUNK)
        .enumerate()
        .map(|(i, part)| {
            DriveMessage {
                message: Some(drive_message::Message::Fragment(DriveFragment {
                    part: part.to_vec(),
                    last: i + 1 == count,
                })),
            }
            .encode_to_vec()
        })
        .collect()
}

/// Why a data channel message does not make a drive message.
#[derive(Debug)]
pub enum ReassembleError {
    Decode(prost::DecodeError),
    /// The fragments add up to more than any drive message should.
    TooLarge,
}

impl std::fmt::Display for ReassembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "undecodable drive message: {e}"),
            Self::TooLarge => f.write_str("drive message too large"),
        }
    }
}

impl std::error::Error for ReassembleError {}

impl From<prost::DecodeError> for ReassembleError {
    fn from(e: prost::DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// Puts fragmented messages back together.
#[derive(Default)]
pub struct Reassembler {
    pending: Vec<u8>,
}

impl Reassembler {
    /// The largest message put back together: a read or write of 1 MiB with room to spare.
    const MAX: usize = 4 * 1024 * 1024;

    /// A data channel message: the drive message it completes, if any.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<DriveMessage>, ReassembleError> {
        let message = DriveMessage::decode(bytes)?;
        let Some(drive_message::Message::Fragment(fragment)) = message.message else {
            return Ok(Some(message));
        };
        self.pending.extend_from_slice(&fragment.part);
        if self.pending.len() > Self::MAX {
            self.pending.clear();
            return Err(ReassembleError::TooLarge);
        }
        if !fragment.last {
            return Ok(None);
        }
        let whole = std::mem::take(&mut self.pending);
        Ok(Some(DriveMessage::decode(whole.as_slice())?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{DriveReply, DriveRequest, DriveWrite, drive_reply, drive_request};

    fn write(size: usize) -> DriveMessage {
        DriveMessage {
            message: Some(drive_message::Message::Request(DriveRequest {
                request_id: 7,
                drive_id: 1,
                operation: Some(drive_request::Operation::Write(DriveWrite {
                    file_id: 3,
                    offset: 0,
                    data: vec![0xAB; size],
                })),
            })),
        }
    }

    #[test]
    fn small_messages_go_whole() {
        let message = DriveMessage {
            message: Some(drive_message::Message::Reply(DriveReply {
                request_id: 1,
                status: 0,
                result: Some(drive_reply::Result::Written(5)),
            })),
        };
        let parts = encode(&message);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            Reassembler::default().push(&parts[0]).unwrap(),
            Some(message)
        );
    }

    #[test]
    fn large_messages_go_in_fragments() {
        let message = write(1024 * 1024);
        let parts = encode(&message);
        assert!(parts.len() > 16);
        assert!(parts.iter().all(|p| p.len() <= MAX_CHUNK + 16));
        let mut reassembler = Reassembler::default();
        let (last, first) = parts.split_last().unwrap();
        for part in first {
            assert_eq!(reassembler.push(part).unwrap(), None);
        }
        assert_eq!(reassembler.push(last).unwrap(), Some(message));
    }
}
