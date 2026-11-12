// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! One server serves every VM of its host: it finds them through their VM manager (libvirt,
//! Proxmox VE, or by hand), attaches to one when a client asks for it, encodes its screens with
//! the host GPU, and serves them over WebRTC.

mod access;
mod agent;
mod audio;
mod cameras;
mod clipboard;
mod config;
mod copied;
mod drives;
mod input;
mod layout;
mod libvirt;
mod pointer;
mod printers;
mod providers;
mod session;
mod tools;
mod vm;
mod vms;
mod web;

use anyhow::Result;
use tracing::{info, warn};

use qdw_video::encode::Codec;
use qdw_video::gpus;

/// How long the sessions have to end when the server stops, and the VMs to be detached then
/// (an encoder takes up to two seconds to finish).
const STOP_TIME: std::time::Duration = std::time::Duration::from_secs(1);
const DETACH_TIME: std::time::Duration = std::time::Duration::from_secs(4);

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if tools::run(&args)? {
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

    let tls = config.tls()?;
    let auth = config.auth()?;
    let vms = vms::Vms::new(config.providers, config.vm);
    info!(providers = ?vms.providers(), "finding the VMs");
    if matches!(auth, config::Auth::None) && !config.listen.ip().is_loopback() {
        warn!(listen = %config.listen, "[auth] method is none: anyone who reaches this address sees the VMs");
    }
    let served = vms.clone();
    tokio::spawn(async move {
        if let Err(e) = web::serve(config.listen, tls, served, auth).await {
            warn!(error = %format!("{e:#}"), "the viewer pages are down");
        }
    });
    // Stopped (Ctrl+C, or systemd): the sessions end cleanly, each telling its client the server
    // stops, before the process goes. Only a crash leaves the clients without a word.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    info!("stopping: the sessions end");
    vms.stop();
    tokio::time::sleep(STOP_TIME).await;
    // Then the VMs, their encoders finished: nothing is left playing.
    if tokio::time::timeout(DETACH_TIME, vms.detach_all())
        .await
        .is_err()
    {
        warn!("the VMs were not all detached in time");
    }
    info!("stopped");
    Ok(())
}
