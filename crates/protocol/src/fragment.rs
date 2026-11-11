// SPDX-License-Identifier: MPL-2.0

//! Messages over a data channel that may not fit in one: those larger than [`MAX_CHUNK`] go in
//! fragments. The drive and printer messages have a fragment variant for this.

use prost::Message;

use crate::MAX_CHUNK;
use crate::v1::{
    DriveFragment, DriveMessage, PrinterFragment, PrinterMessage, drive_message, printer_message,
};

/// A message type with a fragment variant.
pub trait Fragmented: Message + Default + Sized {
    /// The largest message put back together.
    const MAX: usize;
    fn fragment(part: Vec<u8>, last: bool) -> Self;
    /// The fragment this message is, if it is one: `(part, last)`.
    fn into_fragment(self) -> Result<(Vec<u8>, bool), Self>;
}

impl Fragmented for DriveMessage {
    /// A read or write of 1 MiB with room to spare.
    const MAX: usize = 4 * 1024 * 1024;

    fn fragment(part: Vec<u8>, last: bool) -> Self {
        Self {
            message: Some(drive_message::Message::Fragment(DriveFragment {
                part,
                last,
            })),
        }
    }

    fn into_fragment(self) -> Result<(Vec<u8>, bool), Self> {
        match self.message {
            Some(drive_message::Message::Fragment(f)) => Ok((f.part, f.last)),
            message => Err(Self { message }),
        }
    }
}

impl Fragmented for PrinterMessage {
    /// Capabilities and a ticket of 4 MiB each, with room to spare.
    const MAX: usize = 9 * 1024 * 1024;

    fn fragment(part: Vec<u8>, last: bool) -> Self {
        Self {
            message: Some(printer_message::Message::Fragment(PrinterFragment {
                part,
                last,
            })),
        }
    }

    fn into_fragment(self) -> Result<(Vec<u8>, bool), Self> {
        match self.message {
            Some(printer_message::Message::Fragment(f)) => Ok((f.part, f.last)),
            message => Err(Self { message }),
        }
    }
}

/// The data channel messages that carry `message`: itself when it fits, otherwise its encoded
/// bytes in fragments, to send one after the other with nothing in between.
pub fn encode<M: Fragmented>(message: &M) -> Vec<Vec<u8>> {
    let bytes = message.encode_to_vec();
    if bytes.len() <= MAX_CHUNK {
        return vec![bytes];
    }
    let count = bytes.len().div_ceil(MAX_CHUNK);
    bytes
        .chunks(MAX_CHUNK)
        .enumerate()
        .map(|(i, part)| M::fragment(part.to_vec(), i + 1 == count).encode_to_vec())
        .collect()
}

/// Why a data channel message does not make a message.
#[derive(Debug)]
pub enum ReassembleError {
    Decode(prost::DecodeError),
    /// The fragments add up to more than any message should.
    TooLarge,
}

impl std::fmt::Display for ReassembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "undecodable message: {e}"),
            Self::TooLarge => f.write_str("message too large"),
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
pub struct Reassembler<M> {
    pending: Vec<u8>,
    _message: std::marker::PhantomData<M>,
}

impl<M> Default for Reassembler<M> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            _message: std::marker::PhantomData,
        }
    }
}

impl<M: Fragmented> Reassembler<M> {
    /// A data channel message: the message it completes, if any.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<M>, ReassembleError> {
        let (part, last) = match M::decode(bytes)?.into_fragment() {
            Ok(fragment) => fragment,
            Err(message) => return Ok(Some(message)),
        };
        self.pending.extend_from_slice(&part);
        if self.pending.len() > M::MAX {
            self.pending.clear();
            return Err(ReassembleError::TooLarge);
        }
        if !last {
            return Ok(None);
        }
        let whole = std::mem::take(&mut self.pending);
        Ok(Some(M::decode(whole.as_slice())?))
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
            Reassembler::<DriveMessage>::default()
                .push(&parts[0])
                .unwrap(),
            Some(message)
        );
    }

    #[test]
    fn large_messages_go_in_fragments() {
        let message = write(1024 * 1024);
        let parts = encode(&message);
        assert!(parts.len() > 16);
        assert!(parts.iter().all(|p| p.len() <= MAX_CHUNK + 16));
        let mut reassembler = Reassembler::<DriveMessage>::default();
        let (last, first) = parts.split_last().unwrap();
        for part in first {
            assert_eq!(reassembler.push(part).unwrap(), None);
        }
        assert_eq!(reassembler.push(last).unwrap(), Some(message));
    }

    #[test]
    fn printer_messages_too() {
        use crate::v1::{PrinterJobData, printer_message};
        let message = PrinterMessage {
            message: Some(printer_message::Message::JobData(PrinterJobData {
                job_id: 2,
                data: vec![0x5A; 256 * 1024],
            })),
        };
        let parts = encode(&message);
        assert_eq!(parts.len(), 5);
        let mut reassembler = Reassembler::<PrinterMessage>::default();
        let (last, first) = parts.split_last().unwrap();
        for part in first {
            assert_eq!(reassembler.push(part).unwrap(), None);
        }
        assert_eq!(reassembler.push(last).unwrap(), Some(message));
    }
}
