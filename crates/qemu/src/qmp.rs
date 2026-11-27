// SPDX-License-Identifier: MPL-2.0

//! A minimal QMP client: just enough to hand QEMU a peer-to-peer D-Bus display connection
//! (`-display dbus,p2p=on`), through `getfd` and `add_client`, and to run commands without
//! arguments.

use std::io::{BufRead, BufReader, IoSlice, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
use serde_json::{Value, json};

/// How long QEMU has to answer, or to take what is written, on its QMP socket.
const IO_TIME: std::time::Duration = std::time::Duration::from_secs(10);

/// Name under which the display socket is handed to QEMU, then consumed by `add_client`.
const FD_NAME: &str = "qemu-dbus-webrtc";

pub struct Qmp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Qmp {
    /// Connect to a QMP socket and leave capabilities negotiation mode.
    pub fn connect(path: &Path) -> Result<Self> {
        let stream = connect_within(path, IO_TIME)
            .with_context(|| format!("connecting to QMP at {}", path.display()))?;
        // A QEMU that never answers fails the exchange rather than hold its thread for ever.
        stream.set_read_timeout(Some(IO_TIME))?;
        stream.set_write_timeout(Some(IO_TIME))?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut qmp = Self { stream, reader };
        let greeting = qmp.read_message()?;
        if greeting.get("QMP").is_none() {
            bail!("{} did not greet as QMP", path.display());
        }
        qmp.execute("qmp_capabilities", None, None)?;
        Ok(qmp)
    }

    /// Give QEMU one end of a socket pair as a new D-Bus display client. QEMU accepts one such
    /// client at a time: a new one replaces the previous one.
    pub fn add_dbus_display_client(&mut self, socket: OwnedFd) -> Result<()> {
        self.execute("getfd", Some(json!({ "fdname": FD_NAME })), Some(&socket))?;
        // QEMU holds its own copy of the descriptor now.
        drop(socket);
        self.execute(
            "add_client",
            Some(json!({ "protocol": "@dbus-display", "fdname": FD_NAME })),
            None,
        )?;
        Ok(())
    }

    /// Run a command without arguments (`system_reset`, `system_powerdown`, `quit`...).
    pub fn command(&mut self, command: &str) -> Result<()> {
        self.execute(command, None, None)?;
        Ok(())
    }

    /// Run a command and return its result; `fd` goes along with it as SCM_RIGHTS. Asynchronous
    /// events received meanwhile are skipped.
    fn execute(
        &mut self,
        command: &str,
        arguments: Option<Value>,
        fd: Option<&OwnedFd>,
    ) -> Result<Value> {
        let mut message = json!({ "execute": command });
        if let Some(arguments) = arguments {
            message["arguments"] = arguments;
        }
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        match fd {
            Some(fd) => self.send_with_fd(&bytes, fd)?,
            None => self.stream.write_all(&bytes)?,
        }
        loop {
            let reply = self.read_message()?;
            if reply.get("event").is_some() {
                continue;
            }
            if let Some(result) = reply.get("return") {
                return Ok(result.clone());
            }
            if let Some(error) = reply.get("error") {
                bail!(
                    "QMP {command}: {}",
                    error["desc"].as_str().unwrap_or("unknown error")
                );
            }
            bail!("QMP {command}: unexpected reply {reply}");
        }
    }

    fn send_with_fd(&mut self, bytes: &[u8], fd: &OwnedFd) -> Result<()> {
        let fds = [fd.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        if !ancillary.push(SendAncillaryMessage::ScmRights(&fds)) {
            bail!("no room for the file descriptor in the QMP message");
        }
        let sent = sendmsg(
            &self.stream,
            &[IoSlice::new(bytes)],
            &mut ancillary,
            SendFlags::empty(),
        )?;
        // The descriptor went with the first byte; the rest of the command follows as plain data.
        self.stream.write_all(&bytes[sent..])?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("QMP connection closed");
        }
        serde_json::from_str(&line)
            .with_context(|| format!("unreadable QMP message: {}", line.trim_end()))
    }
}

/// Connect to the Unix socket at `path`, waiting at most `time` for room in its backlog: a
/// QEMU that stopped accepting (its main loop stuck) fails the connection rather than hold the
/// caller's thread for ever. Linux bounds a Unix socket's connect by its send timeout.
fn connect_within(path: &Path, time: std::time::Duration) -> std::io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, sockopt};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    sockopt::set_socket_timeout(&socket, sockopt::Timeout::Send, Some(time))?;
    rustix::net::connect(&socket, &SocketAddrUnix::new(path)?)?;
    Ok(UnixStream::from(socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_that_takes_nobody_fails_the_connection() {
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
        let dir = std::env::temp_dir().join(format!("qdw-qmp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qmp.sock");
        let _ = std::fs::remove_file(&path);
        // A listener that accepts nothing, with no room beyond one waiting connection.
        let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::net::bind(&listener, &SocketAddrUnix::new(&path).unwrap()).unwrap();
        rustix::net::listen(&listener, 0).unwrap();
        let time = std::time::Duration::from_millis(200);
        // The backlog fills, then a connection waits for room: at most `time`.
        let mut waiting = Vec::new();
        let failed = loop {
            let started = std::time::Instant::now();
            match connect_within(&path, time) {
                Ok(stream) => waiting.push(stream),
                Err(e) => break (e, started.elapsed()),
            }
            assert!(waiting.len() < 16, "the backlog never filled");
        };
        assert!(failed.1 >= time && failed.1 < time * 10, "{failed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
