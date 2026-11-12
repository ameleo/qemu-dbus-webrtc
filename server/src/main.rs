// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! One server serves every VM of its host: it finds them through their VM manager (libvirt,
//! Proxmox VE, or by hand), attaches to one when a client asks for it, encodes its screens with
//! the host GPU, and serves them over WebRTC.

mod agent;
mod audio;
mod cameras;
mod clipboard;
mod config;
mod copied;
mod drives;
mod input;
mod layout;
mod pointer;
mod printers;
mod providers;
mod session;
mod vm;
mod vms;
mod web;

use anyhow::Result;
use tracing::{info, warn};

use qdw_video::encode::Codec;
use qdw_video::gpus;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if std::env::args().nth(1).as_deref() == Some("--codecs") {
        gstreamer::init()?;
        print!("{}", gpus::report(&gpus::gpus(), gpus::software()));
        return Ok(());
    }
    let mut config = config::parse_args()?;
    gstreamer::init()?;
    let found = gpus::gpus();
    let vm = &mut config.vm;
    // Without a GPU, encode on the CPU: VP9, unless another codec was asked for.
    if found.is_empty() && !vm.settings.software {
        info!("no GPU encoder: encoding on the CPU");
        vm.settings.software = true;
    }
    if vm.settings.software && !vm.codec_given {
        vm.settings.codec = Codec::Vp9;
    }
    // Fail now, saying why, rather than at the first frame.
    gpus::check(&found, &vm.settings)?;

    let vms = vms::Vms::new(config.providers, config.vm);
    info!(providers = ?vms.providers(), "finding the VMs");
    if !config.listen.ip().is_loopback() {
        warn!(listen = %config.listen, "no authentication yet: anyone who reaches this address sees the VMs");
    }
    tokio::spawn(async move {
        if let Err(e) = web::serve(config.listen, config.tls, vms).await {
            warn!(error = %format!("{e:#}"), "the viewer pages are down");
        }
    });
    tokio::signal::ctrl_c().await?;
    Ok(())
}
