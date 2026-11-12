// SPDX-License-Identifier: MPL-2.0

//! The Ameleo guest agent, reached through a QEMU character device on the D-Bus display.
//!
//! QEMU exports the character device as `org.qemu.Display1.Chardev`; `Register` hands it one end
//! of a socket pair. Over it runs a peer-to-peer D-Bus connection on which the guest agent is
//! the server and we are the client (anonymous authentication). The guest exports its
//! interfaces at `/fr/ameleo/Agent`, we export ours at `/fr/ameleo/Host`.
//!
//! Only what this program uses of the agent's contract is declared here: the core interface,
//! the clipboard (files included), the client drives, printers and cameras.

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
    /// Files and directories: the Windows format name, whose data is a CLIPRDR_FILELIST (see
    /// [`super::files`]); the contents come through `FileClipboard1`.
    pub const FILE_LIST: &str = "FileGroupDescriptorW";
}

/// `FileContents` flags (MS-RDPECLIP 2.2.5.3).
pub mod file_contents {
    /// Return the file size, 8 bytes little endian.
    pub const SIZE: u32 = 0x1;
    /// Return the requested bytes from the position.
    pub const RANGE: u32 = 0x2;
    /// The largest piece the agent returns in one call.
    pub const MAX_RANGE: u32 = 4 * 1024 * 1024;
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

/// Files copied in the guest (MS-RDPECLIP File Contents Request and Lock/Unlock Clipboard Data):
/// `lindex` is the index in the file list of the grab, `flags` [`file_contents::SIZE`] or
/// [`file_contents::RANGE`]; with `has_clip_data_id`, the request is about the content locked
/// under `clip_data_id`, otherwise the current one. The host exports the same calls as
/// `fr.ameleo.Host.FileClipboard1`, for files copied on the client.
#[zbus::proxy(
    interface = "fr.ameleo.Agent.FileClipboard1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait FileClipboard {
    fn lock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    fn unlock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> zbus::Result<Vec<u8>>;
}

/// Client drives shown in the guest as drive letters (RDP drive redirection, MS-RDPEFS). The
/// guest mounts each with Dokany, and sends every file operation of its applications to the
/// host as it happens (`fr.ameleo.Host.Drive1`). Ids are chosen by the host. Paths are relative
/// to the drive root, `\` separated, without a leading `\`.
#[zbus::proxy(
    interface = "fr.ameleo.Agent.Drive1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait Drive {
    /// `name`: the client's name of the drive (`C`); `label`: the volume label shown in the
    /// guest. Returns the guest's drive letter.
    fn add_drive(&self, id: u32, name: &str, label: &str) -> zbus::Result<String>;
    fn remove_drive(&self, id: u32) -> zbus::Result<()>;
    /// Changes made on the client drive, in order: `(action, path)`, the
    /// `FILE_NOTIFY_INFORMATION` actions.
    fn changed(&self, id: u32, changes: Vec<(u32, String)>) -> zbus::Result<()>;
}

/// Drive limits and wire types of the agent's contract.
pub mod drive {
    /// Client drives shown at the same time.
    pub const MAX_DRIVES: usize = 24;
    /// Largest read length or write data, in bytes.
    pub const IO_MAX: u32 = 1024 * 1024;
    /// A file or directory: `(name, attributes, size, creation_time, last_access_time,
    /// last_write_time, change_time)`, times as FILETIME.
    pub type Entry = (String, u32, u64, u64, u64, u64, u64);
}

/// Client printers shown in the guest as Windows printers (RDP Easy Print): virtual printers
/// of the guest's Print Support App, which hands each job to the host as OpenXPS
/// (`fr.ameleo.Host.Printer1`). Ids are chosen by the host.
#[zbus::proxy(
    interface = "fr.ameleo.Agent.Printer1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait Printer {
    /// `capabilities`: the client driver's PrintCapabilities XML; `default_ticket`: the
    /// PrintTicket of its default settings. Either may be empty. Returns at once; the guest
    /// installs the printer for the logged on user, at once or when the user logs on.
    fn add_printer(
        &self,
        id: u32,
        name: &str,
        is_default: bool,
        capabilities: &str,
        default_ticket: &str,
    ) -> zbus::Result<()>;
    fn remove_printer(&self, id: u32) -> zbus::Result<()>;
    /// Answer to `Host.Printer1.DocumentProperties`: the PrintTicket the user chose in the
    /// client driver's dialog, or `cancelled`.
    fn document_properties_done(
        &self,
        request_id: u32,
        print_ticket: &str,
        cancelled: bool,
    ) -> zbus::Result<()>;
}

/// Printer limits of the agent's contract.
pub mod printer {
    /// Client printers shown at the same time.
    pub const MAX_PRINTERS: usize = 32;
    /// Largest PrintCapabilities or PrintTicket, in bytes.
    pub const MAX_XML: usize = 4 * 1024 * 1024;
}

/// Client cameras shown in the guest as Windows cameras (RDP camera redirection, MS-RDPECAM).
/// The host announces each with the H.264 formats it sends; when an application opens it, the
/// guest asks for a format, and the host pushes each H.264 access unit (Annex B, SPS and PPS
/// before each IDR). Ids are chosen by the host.
#[zbus::proxy(
    interface = "fr.ameleo.Agent.Camera1",
    default_service = "fr.ameleo.Agent",
    default_path = "/fr/ameleo/Agent"
)]
pub trait Camera {
    /// `formats`: `(width, height, fps numerator, fps denominator)`.
    fn add_camera(
        &self,
        id: u32,
        name: &str,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> zbus::Result<()>;
    fn remove_camera(&self, id: u32) -> zbus::Result<()>;
    /// One access unit, in the format of the last `StreamRequested`; `timestamp_us`: capture
    /// time on the client.
    #[zbus(no_reply)]
    fn push_frame(&self, id: u32, timestamp_us: u64, data: Vec<u8>) -> zbus::Result<()>;
    /// An application opened camera `id` in format `format` (an index in `AddCamera`).
    #[zbus(signal)]
    fn stream_requested(&self, id: u32, format: u32) -> zbus::Result<()>;
    /// No application uses camera `id` anymore.
    #[zbus(signal)]
    fn stream_stopped(&self, id: u32) -> zbus::Result<()>;
    /// The guest needs a keyframe to decode camera `id` again.
    #[zbus(signal)]
    fn key_frame_needed(&self, id: u32) -> zbus::Result<()>;
    /// The controls of camera `id`, replacing any given before.
    fn set_controls(&self, id: u32, controls: Vec<camera::Control>) -> zbus::Result<()>;
    /// The value a control of camera `id` has now.
    fn control_value(
        &self,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;
    /// An application changed a control: `value` means nothing in the auto mode.
    #[zbus(signal)]
    fn control_requested(
        &self,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;
}

/// Camera limits and wire types of the agent's contract.
pub mod camera {
    /// Client cameras shown at the same time.
    pub const MAX_CAMERAS: usize = 4;
    /// Largest frame size.
    pub const MAX_WIDTH: u32 = 3840;
    pub const MAX_HEIGHT: u32 = 2160;
    /// Controls given at once.
    pub const MAX_CONTROLS: usize = 16;
    /// `(set, property, capabilities, min, max, step, default, mode, value)`, numbered as in
    /// MS-RDPECAM.
    pub type Control = (u8, u8, u8, i32, i32, i32, i32, u8, i32);
}

/// Connect to the guest agent. `host` adds our interfaces (the calls the guest makes) to the
/// connection, at [`HOST_PATH`]. Returns once the agent answered the handshake, so not before
/// the guest runs it.
pub async fn connect(
    vm: &zbus::Connection,
    host: impl FnOnce(
        zbus::connection::Builder<'static>,
    ) -> zbus::Result<zbus::connection::Builder<'static>>,
) -> Result<zbus::Connection> {
    let chardev = find_chardev(vm).await?;
    let (ours, theirs) = StdUnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    let connection = host(
        zbus::connection::Builder::unix_stream(stream)
            .p2p()
            .auth_mechanism(zbus::AuthMechanism::Anonymous),
    )?
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
