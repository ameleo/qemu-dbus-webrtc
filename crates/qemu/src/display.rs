// SPDX-License-Identifier: MPL-2.0

//! QEMU's D-Bus display (`-display dbus`, interfaces in QEMU's `ui/dbus-display1.xml`).
//!
//! Two ways to reach a VM:
//! - peer to peer (`p2p=on`): a socket handed to QEMU over QMP (`add_client`). QEMU serves one
//!   such client at a time.
//! - a bus (`addr=...`), typically a private bus per VM: QEMU owns `org.qemu` on it, and several
//!   clients (consoles of different kinds) can attach side by side.
//!
//! Either way, display updates come through a listener: a socket pair per console, one end given
//! to QEMU with `RegisterListener`, on which we serve `org.qemu.Display1.Listener`.

use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use zbus::zvariant::OwnedFd;

use crate::qmp::Qmp;

/// How to reach the VM's display.
#[derive(Debug, Clone)]
pub enum Attach {
    /// Peer to peer, through the VM's QMP socket.
    P2p { qmp: PathBuf },
    /// A D-Bus bus on which QEMU owns `org.qemu`.
    Bus { address: String },
}

#[zbus::proxy(
    interface = "org.qemu.Display1.VM",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/VM"
)]
pub trait Vm {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property, name = "ConsoleIDs")]
    fn console_ids(&self) -> zbus::Result<Vec<u32>>;
}

#[zbus::proxy(interface = "org.qemu.Display1.Console", default_service = "org.qemu")]
pub trait Console {
    fn register_listener(&self, listener: OwnedFd) -> zbus::Result<()>;
    /// Ask the guest for a screen of `width` x `height` pixels, `width_mm` x `height_mm` large,
    /// at `xoff`, `yoff` among the others (the guest's display driver applies it, or not).
    #[zbus(name = "SetUIInfo")]
    fn set_ui_info(
        &self,
        width_mm: u16,
        height_mm: u16,
        xoff: i32,
        yoff: i32,
        width: u32,
        height: u32,
    ) -> zbus::Result<()>;
    #[zbus(property)]
    fn label(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn head(&self) -> zbus::Result<u32>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn width(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn height(&self) -> zbus::Result<u32>;
}

/// Connect to the VM's display.
pub async fn attach(how: &Attach) -> Result<zbus::Connection> {
    match how {
        Attach::P2p { qmp } => {
            let (ours, theirs) = StdUnixStream::pair()?;
            let qmp = qmp.clone();
            // QMP is a short blocking exchange; keep it off the async threads.
            tokio::task::spawn_blocking(move || {
                Qmp::connect(&qmp)?.add_dbus_display_client(theirs.into())
            })
            .await??;
            ours.set_nonblocking(true)?;
            let stream = tokio::net::UnixStream::from_std(ours)?;
            // QEMU is the authenticating side of the connection.
            Ok(zbus::connection::Builder::unix_stream(stream)
                .p2p()
                .build()
                .await?)
        }
        Attach::Bus { address } => Ok(zbus::connection::Builder::address(address.as_str())?
            .build()
            .await
            .with_context(|| format!("connecting to D-Bus at {address}"))?),
    }
}

/// What the guest display does, per console.
#[derive(Debug)]
pub enum DisplayEvent {
    /// A new framebuffer, copied in the call (no shared memory, no GPU sharing): its full
    /// content.
    Scanout {
        console: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    },
    /// Part of the copied framebuffer changed: the pixels of the rectangle.
    Update {
        console: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    },
    /// A new framebuffer in memory shared with QEMU (a guest without GPU acceleration): QEMU
    /// draws in it, and says which part changed with `UpdateMap`.
    ScanoutMap {
        console: u32,
        fd: std::os::fd::OwnedFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
    },
    /// Part of the shared framebuffer changed.
    UpdateMap {
        console: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// A new framebuffer in GPU memory (DMA-BUF), one descriptor per plane.
    ScanoutDmabuf {
        console: u32,
        planes: Vec<Plane>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    },
    /// Part of the DMA-BUF framebuffer changed.
    UpdateDmabuf {
        console: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// The head was turned off.
    Disable { console: u32 },
    /// Pointer position and visibility.
    MouseSet {
        console: u32,
        x: i32,
        y: i32,
        visible: bool,
    },
    /// Pointer shape, ARGB 32 bits per pixel.
    CursorDefine {
        console: u32,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: Vec<u8>,
    },
}

#[derive(Debug)]
pub struct Plane {
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// `org.qemu.Display1.Listener` for one console.
struct Listener {
    console: u32,
    events: mpsc::UnboundedSender<DisplayEvent>,
}

impl Listener {
    fn send(&self, event: DisplayEvent) {
        // The receiver is gone only when shutting down.
        let _ = self.events.send(event);
    }
}

// Calls are handled one at a time, in the order QEMU sends them: a scanout must not overtake
// the one before it, nor the updates that follow it.
#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl Listener {
    async fn scanout(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    ) {
        self.send(DisplayEvent::Scanout {
            console: self.console,
            width,
            height,
            stride,
            pixman_format,
            data,
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn update(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: u32,
        pixman_format: u32,
        data: Vec<u8>,
    ) {
        self.send(DisplayEvent::Update {
            console: self.console,
            x,
            y,
            width,
            height,
            stride,
            pixman_format,
            data,
        });
    }

    #[zbus(name = "ScanoutDMABUF")]
    #[allow(clippy::too_many_arguments)]
    async fn scanout_dmabuf(
        &self,
        dmabuf: OwnedFd,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) {
        let planes = vec![Plane {
            fd: dmabuf.into(),
            offset: 0,
            stride,
        }];
        self.send(DisplayEvent::ScanoutDmabuf {
            console: self.console,
            planes,
            width,
            height,
            fourcc,
            modifier,
            y0_top,
        });
    }

    #[zbus(name = "UpdateDMABUF")]
    async fn update_dmabuf(&self, x: i32, y: i32, width: i32, height: i32) {
        self.send(DisplayEvent::UpdateDmabuf {
            console: self.console,
            x,
            y,
            width,
            height,
        });
    }

    async fn disable(&self) {
        self.send(DisplayEvent::Disable {
            console: self.console,
        });
    }

    async fn mouse_set(&self, x: i32, y: i32, on: i32) {
        self.send(DisplayEvent::MouseSet {
            console: self.console,
            x,
            y,
            visible: on != 0,
        });
    }

    async fn cursor_define(&self, width: i32, height: i32, hot_x: i32, hot_y: i32, data: Vec<u8>) {
        self.send(DisplayEvent::CursorDefine {
            console: self.console,
            width,
            height,
            hot_x,
            hot_y,
            data,
        });
    }

    /// The optional interfaces served next to this one. ScanoutDMABUF2 describes buffers that
    /// ScanoutDMABUF cannot (several planes, offsets), so QEMU uses it when we announce it; with
    /// Map, QEMU shares its framebuffer when it is in memory instead of copying its pixels in
    /// each call.
    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        vec![
            "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2".to_owned(),
            "org.qemu.Display1.Listener.Unix.Map".to_owned(),
        ]
    }
}

/// `org.qemu.Display1.Listener.Unix.ScanoutDMABUF2` for one console.
struct ScanoutDmabuf2 {
    console: u32,
    events: mpsc::UnboundedSender<DisplayEvent>,
}

#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2", spawn = false)]
impl ScanoutDmabuf2 {
    #[zbus(name = "ScanoutDMABUF2")]
    #[allow(clippy::too_many_arguments)]
    async fn scanout_dmabuf2(
        &self,
        dmabuf: Vec<OwnedFd>,
        _x: u32,
        _y: u32,
        width: u32,
        height: u32,
        offset: Vec<u32>,
        stride: Vec<u32>,
        num_planes: u32,
        fourcc: u32,
        _backing_width: u32,
        _backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) {
        let planes = dmabuf
            .into_iter()
            .zip(offset)
            .zip(stride)
            .take(num_planes as usize)
            .map(|((fd, offset), stride)| Plane {
                fd: fd.into(),
                offset,
                stride,
            })
            .collect();
        let _ = self.events.send(DisplayEvent::ScanoutDmabuf {
            console: self.console,
            planes,
            width,
            height,
            fourcc,
            modifier,
            y0_top,
        });
    }
}

/// `org.qemu.Display1.Listener.Unix.Map` for one console.
struct Map {
    console: u32,
    events: mpsc::UnboundedSender<DisplayEvent>,
}

#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map", spawn = false)]
impl Map {
    async fn scanout_map(
        &self,
        handle: OwnedFd,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixman_format: u32,
    ) {
        let _ = self.events.send(DisplayEvent::ScanoutMap {
            console: self.console,
            fd: handle.into(),
            offset,
            width,
            height,
            stride,
            pixman_format,
        });
    }

    async fn update_map(&self, x: i32, y: i32, width: i32, height: i32) {
        let _ = self.events.send(DisplayEvent::UpdateMap {
            console: self.console,
            x,
            y,
            width,
            height,
        });
    }
}

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";

/// Register a listener on console `id`. The returned connection carries the display updates:
/// keep it alive for as long as they are wanted.
pub async fn listen(
    vm: &zbus::Connection,
    id: u32,
    events: mpsc::UnboundedSender<DisplayEvent>,
) -> Result<zbus::Connection> {
    let console = ConsoleProxy::builder(vm)
        .path(format!("/org/qemu/Display1/Console_{id}"))?
        .build()
        .await?;
    let (ours, theirs) = StdUnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    let connection = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(
            LISTENER_PATH,
            Listener {
                console: id,
                events: events.clone(),
            },
        )?
        .serve_at(
            LISTENER_PATH,
            ScanoutDmabuf2 {
                console: id,
                events: events.clone(),
            },
        )?
        .serve_at(
            LISTENER_PATH,
            Map {
                console: id,
                events,
            },
        )?
        .build();
    // QEMU authenticates the listener connection before it answers RegisterListener: run both
    // at once.
    let theirs: std::os::fd::OwnedFd = theirs.into();
    let (connection, registered) =
        tokio::join!(connection, console.register_listener(theirs.into()));
    registered.with_context(|| format!("registering a listener on console {id}"))?;
    Ok(connection?)
}
