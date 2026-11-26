// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! One server serves every VM of its host: it finds them through their VM manager (libvirt,
//! Proxmox VE, or by hand), attaches to one when a client asks for it, encodes its screens with
//! the host GPU, and serves them over WebRTC.

mod access;
mod acme;
mod agent;
mod attestation;
mod audio;
mod broker;
mod cameras;
mod clipboard;
mod config;
mod copied;
mod drives;
mod input;
mod layout;
mod libvirt;
mod lock;
mod peer;
mod pointer;
mod printers;
mod providers;
mod stun;
mod tools;
mod transport;
mod turn;
mod vm;
mod vms;
mod web;

use anyhow::{Context, Result};
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
        print!("{}", gpus::report(&gpus::gpus()));
        return Ok(());
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if tools::run(&args)? {
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("enroll") {
        return enroll(&args[1..]).await;
    }
    let mut config = config::parse_args()?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("QDW_COMMIT"),
        "starting qemu-dbus-webrtc"
    );
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

    // With a broker, the host serves its VMs through it only, and opens no port at all: the
    // broker relays its users (the configuration refuses anything else beside [broker]).
    let pages = if config.serves_pages() {
        Some((config.tls()?, config.auth()?))
    } else {
        info!("a broker: the host serves its VMs through it only");
        None
    };
    let vms = vms::Vms::new(config.providers, config.vm);
    info!(providers = ?vms.providers(), "finding the VMs");
    if let Some(settings) = config.broker {
        tokio::spawn(broker::keep(settings, vms.clone()));
    }
    let mut serving = match pages {
        Some((tls, auth)) => {
            serve_pages(config.listen, tls, auth, config.acme, vms.clone()).await?
        }
        // Nothing to serve: only a signal stops the server.
        None => tokio::spawn(std::future::pending()),
    };
    // Stopped (Ctrl+C, or systemd): the sessions end cleanly, each telling its client the server
    // stops, before the process goes. Only a crash leaves the clients without a word.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
        // Not serving (the address taken, the certificate unreadable): a server that serves
        // nothing fails, and its service manager sees it.
        served = &mut serving => {
            let error = match served {
                Ok(Ok(())) => anyhow::anyhow!("the viewer pages stopped"),
                Ok(Err(e)) => e,
                Err(e) => anyhow::anyhow!("the viewer pages failed: {e}"),
            };
            vms.stop();
            return Err(error.context("serving the viewer pages"));
        }
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

/// Serve the VMs' own pages on `listen`, with `tls` (from ACME first, when `acme` gives it and
/// none in place can serve) and `auth`.
async fn serve_pages(
    listen: std::net::SocketAddr,
    tls: web::Tls,
    auth: config::Auth,
    acme: Option<acme::Acme>,
    vms: std::sync::Arc<vms::Vms>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    if matches!(auth, config::Auth::None) && !listen.ip().is_loopback() {
        warn!(%listen, "[auth] method is none: anyone who reaches this address sees the VMs");
    }
    // The certificate from ACME first, when none in place can serve (none, expired, for other
    // names, or without its key): the pages need one. One that can serve, even due for renewal,
    // serves at once, and is renewed while it does (`acme::keep`): the start does not wait for
    // the authority.
    if let Some(acme) = &acme
        && !acme.usable()
    {
        acme.renew().await.context("no certificate")?;
    }
    let rustls = web::load_tls(&tls).await?;
    if let Some(acme) = acme {
        let (rustls, tls) = (rustls.clone(), tls.clone());
        tokio::spawn(acme::keep(acme, move || {
            let (rustls, tls) = (rustls.clone(), tls.clone());
            tokio::spawn(async move {
                match rustls
                    .reload_from_pem_file(&tls.certificate, &tls.key)
                    .await
                {
                    Ok(()) => info!("the pages serve the new certificate"),
                    Err(e) => warn!(error = %e, "the new certificate is not served"),
                }
            });
        }));
    }
    Ok(tokio::spawn(web::serve(listen, rustls, vms, auth)))
}

/// `qemu-dbus-webrtc enroll --config <file> [<enrollment kid>]`: the host's certificate from its
/// broker's ACME server ([broker] acme_directory). The first time, with the enrollment token
/// the broker's administrator made (`ameleo-broker --enroll-host`): its key identifier here,
/// its key asked on the terminal, kept out of the command line. Later, without it, the account
/// already bound renews the certificate.
async fn enroll(args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: qemu-dbus-webrtc enroll --config <file.toml> [<enrollment kid>]";
    let (path, kid) = match args {
        [option, path] if option == "--config" => (path, None),
        [option, path, kid] if option == "--config" => (path, Some(kid.clone())),
        _ => anyhow::bail!(USAGE),
    };
    let config = config::parse(vec!["--config".into(), path.clone()])?;
    let Some(mut settings) = config.attestation else {
        anyhow::bail!("[broker] acme_directory is not set: no certificate to get from the broker");
    };
    if let Some(kid) = kid {
        let key = rpassword::prompt_password("enrollment key: ")?;
        let key = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            key.trim().trim_end_matches('='),
        )
        .map_err(|_| anyhow::anyhow!("the enrollment key is base64url"))?;
        settings.enrollment = Some((kid, key));
    }
    attestation::enroll(&settings).await
}
