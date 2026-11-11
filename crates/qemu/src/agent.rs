// SPDX-License-Identifier: MPL-2.0

//! The Ameleo guest agent, reached through a QEMU character device on the D-Bus display.
//!
//! QEMU exports the character device as `org.qemu.Display1.Chardev`; `Register` hands it one end
//! of a socket pair. Over it runs a peer-to-peer D-Bus connection on which the guest agent is
//! the server and we are the client (anonymous authentication). The guest exports its
//! interfaces at `/fr/ameleo/Agent`, we export ours at `/fr/ameleo/Host`.
//!
//! Only what this program uses of the agent's contract is declared here: the core interface
//! and the clipboard.

use std::os::unix::net::UnixStream as StdUnixStream;

use anyhow::{Context, Result};
use zbus::zvariant::OwnedFd;

/// The name of the agent's character device (`-chardev dbus,name=...`).
pub const PORT_NAME: &str = "fr.ameleo.agent.0";

/// Where we export the interfaces the guest calls back.
pub const HOST_PATH: &str = "/fr/ameleo/Host";

/// Clipboard media types the agent understands.
pub mod mime {
    pub const TEXT: &str = "text/plain;charset=utf-8";
    pub const PNG: &str = "image/png";
}

#[zbus::proxy(interface = "org.qemu.Display1.Chardev", default_service = "org.qemu")]
trait Chardev {
    fn register(&self, stream: OwnedFd) -> zbus::Result<()>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "fr.ameleo.Agent1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait Agent {
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
    /// The channel interfaces the agent implements.
    #[zbus(property)]
    fn channels(&self) -> zbus::Result<Vec<String>>;
}

/// The guest clipboard. Whoever copied last owns the clipboard and announces the media types
/// it can provide; the other side asks for the data when an application pastes.
#[zbus::proxy(
    interface = "fr.ameleo.Agent.Clipboard1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait Clipboard {
    /// The host now owns the clipboard: the guest will ask `Host.Clipboard1.Request` for the
    /// data, with this serial.
    fn grab(&self, serial: u32, mimes: Vec<String>) -> zbus::Result<()>;
    /// The host no longer owns the clipboard.
    fn release(&self) -> zbus::Result<()>;
    /// The guest clipboard's data, in the first of `mimes` it can provide.
    fn request(&self, mimes: Vec<String>) -> zbus::Result<(String, Vec<u8>)>;
    /// The guest clipboard changed.
    #[zbus(signal)]
    fn grabbed(&self, serial: u32, mimes: Vec<String>) -> zbus::Result<()>;
    /// The guest clipboard holds nothing to share anymore.
    #[zbus(signal)]
    fn released(&self) -> zbus::Result<()>;
}

/// Connect to the guest agent, exporting `host` (our callbacks) at [`HOST_PATH`]. Returns once
/// the agent answered the handshake, so not before the guest runs it.
pub async fn connect<I>(vm: &zbus::Connection, host: I) -> Result<zbus::Connection>
where
    I: zbus::object_server::Interface,
{
    let chardev = find_chardev(vm).await?;
    let (ours, theirs) = StdUnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    let connection = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .auth_mechanism(zbus::AuthMechanism::Anonymous)
        .serve_at(HOST_PATH, host)?
        .build();
    // The handshake goes through QEMU once it holds the other end: both at once.
    let theirs: std::os::fd::OwnedFd = theirs.into();
    let (connection, registered) = tokio::join!(connection, chardev.register(theirs.into()));
    registered.context("handing the agent's character device a stream")?;
    connection.context("D-Bus handshake with the guest agent")
}

/// The character device of the agent: QEMU names its object after the device's label, so
/// look it up by its name.
async fn find_chardev(vm: &zbus::Connection) -> Result<ChardevProxy<'static>> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(vm)
        .destination("org.qemu")?
        .path("/org/qemu/Display1")?
        .build()
        .await?;
    for (path, interfaces) in manager.get_managed_objects().await? {
        if !interfaces
            .keys()
            .any(|i| i.as_str() == "org.qemu.Display1.Chardev")
        {
            continue;
        }
        let chardev = ChardevProxy::builder(vm).path(path)?.build().await?;
        if chardev.name().await? == PORT_NAME {
            return Ok(chardev);
        }
    }
    anyhow::bail!("the VM has no {PORT_NAME} character device")
}
