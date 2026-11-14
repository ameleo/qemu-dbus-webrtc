// SPDX-License-Identifier: MPL-2.0

//! What can go wrong.

use std::fmt;

/// An encoding error.
#[derive(Debug)]
pub enum Error {
    /// The GPU's device could not be opened.
    Device(std::io::Error),
    /// A call to the GPU's library failed.
    Va {
        call: &'static str,
        status: i32,
        message: String,
    },
    /// The GPU, or its driver, does not do what the settings ask.
    Unsupported(String),
    /// The frame given is not one that can be encoded.
    Frame(String),
    /// The encoded frame did not fit its buffer and is lost. The next one is a keyframe.
    Overflow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => write!(f, "opening the GPU: {error}"),
            Self::Va {
                call,
                status,
                message,
            } => write!(f, "{call}: {message} ({status:#x})"),
            Self::Unsupported(what) => write!(f, "not supported by this GPU: {what}"),
            Self::Frame(what) => write!(f, "bad frame: {what}"),
            Self::Overflow => f.write_str("the encoded frame outgrew its buffer"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Device(error) => Some(error),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
