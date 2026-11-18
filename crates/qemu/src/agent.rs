// SPDX-License-Identifier: MPL-2.0

//! The guest agent, reached through a QEMU character device on the D-Bus display.
//!
//! QEMU exports the character device as `org.qemu.Display1.Chardev`; `Register` hands it one end
//! of a socket pair. Over it runs a peer-to-peer D-Bus connection on which the guest agent is
//! the server and we are the client (anonymous authentication). The guest exports its
//! interfaces at `/fr/ameleo/Agent`, we export ours at `/fr/ameleo/Host`.
//!
//! The agent's contract (its port, paths, interfaces and their constants) comes from
//! `ameleo-display-protocol`, the crate the agent itself is built on.

use std::os::unix::net::UnixStream as StdUnixStream;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use zbus::OwnedFd;

pub use ameleo_display_protocol::{
    AgentProxy, CameraProxy, ClipboardProxy, DriveProxy, FileClipboardProxy, HOST_PATH, MouseProxy,
    PORT_NAME, PrinterProxy, camera, camera_control, drive, file_contents, iface, mime, printer,
};

/// How long the agent has to answer the D-Bus handshake, once the guest opened its port.
const HANDSHAKE_TIME: std::time::Duration = std::time::Duration::from_secs(30);
/// The longest a call to the agent may take: a stuck agent, its port still open, never answers,
/// and the call would hold its caller (a viewer's loop) for ever. Generous: installing a printer
/// in the guest takes seconds.
const CALL_TIME: std::time::Duration = std::time::Duration::from_secs(60);

/// The VM has no agent character device: no agent will ever come.
#[derive(Debug)]
pub struct NoDevice;

impl std::fmt::Display for NoDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the VM has no {PORT_NAME} character device")
    }
}

impl std::error::Error for NoDevice {}

#[zbus::proxy(interface = "org.qemu.Display1.Chardev", default_service = "org.qemu")]
trait Chardev {
    fn register(&self, stream: OwnedFd) -> zbus::Result<()>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    /// Whether the guest has the port open: the agent opens it when it starts, and closes it
    /// when it stops.
    #[zbus(property, name = "FEOpened")]
    fn fe_opened(&self) -> zbus::Result<bool>;
}

pub struct Agent {
    pub connection: zbus::Connection,
    /// The changes of the port's properties, followed from before the handshake.
    port: zbus::fdo::PropertiesChangedStream,
}

impl Agent {
    /// Wait until the guest closes the agent's port: the agent stopped. Its connection then
    /// is over, though QEMU keeps our end of the stream open and nothing fails on it: a new
    /// agent waits for a new handshake.
    pub async fn closed(&mut self) {
        wait_port(&mut self.port, false).await;
    }
}

/// Wait until the port's `FEOpened` changes to `open`, or the VM's connection ends.
async fn wait_port(port: &mut zbus::fdo::PropertiesChangedStream, open: bool) {
    while let Some(signal) = port.next().await {
        let Ok(args) = signal.args() else { continue };
        if let Some(value) = args.changed_properties().get("FEOpened")
            && bool::try_from(value).ok() == Some(open)
        {
            return;
        }
    }
}

/// Connect to the guest agent. `host` adds our interfaces (the calls the guest makes) to the
/// connection, at [`HOST_PATH`]. Returns once the guest has opened the agent's port and the
/// agent answered the handshake, so not before the guest runs it; fails with [`NoDevice`] when
/// the VM has no agent device.
pub async fn connect(
    vm: &zbus::Connection,
    host: impl FnOnce(zbus::connection::Builder<'static>) -> zbus::connection::Builder<'static>,
) -> Result<Agent> {
    let chardev = find_chardev(vm).await?;
    let mut port = zbus::fdo::PropertiesProxy::builder(vm)
        .destination("org.qemu")
        .path(chardev.inner().path().to_owned())
        .build()
        .await?
        .receive_properties_changed()
        .await?;
    if !chardev.fe_opened().await? {
        wait_port(&mut port, true).await;
    }
    let (ours, theirs) = StdUnixStream::pair()?;
    let connection = host(
        zbus::connection::Builder::unix_stream(ours)
            .p2p()
            .auth_mechanism(zbus::connection::AuthMechanism::Anonymous)
            .method_timeout(CALL_TIME),
    )
    .build();
    // The handshake goes through QEMU once it holds the other end: both at once. An agent that
    // opened its port but never answers (stuck, or of another version) is given up after
    // `HANDSHAKE_TIME`: the caller tries again, rather than wait for ever.
    let theirs: std::os::fd::OwnedFd = theirs.into();
    let handshake = async { tokio::join!(connection, chardev.register(theirs.into())) };
    let (connection, registered) = tokio::time::timeout(HANDSHAKE_TIME, handshake)
        .await
        .context("the guest agent did not answer the D-Bus handshake")?;
    registered.context("handing the agent's character device a stream")?;
    let connection = connection.context("D-Bus handshake with the guest agent")?;
    Ok(Agent { connection, port })
}

/// The character device of the agent: QEMU names its object after the device's label, so
/// look it up by its name.
async fn find_chardev(vm: &zbus::Connection) -> Result<ChardevProxy<'static>> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(vm)
        .destination("org.qemu")
        .path("/org/qemu/Display1")
        .build()
        .await?;
    for (path, interfaces) in manager.get_managed_objects().await? {
        if !interfaces
            .keys()
            .any(|i| i.as_str() == "org.qemu.Display1.Chardev")
        {
            continue;
        }
        // Not cached: whether the port is open is asked when needed, never remembered.
        let chardev = ChardevProxy::builder(vm)
            .path(path)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        if chardev.name().await? == PORT_NAME {
            return Ok(chardev);
        }
    }
    Err(NoDevice.into())
}
