// SPDX-License-Identifier: MPL-2.0

//! The server's settings: from a configuration file (`--config`, TOML), one per host, then from
//! the command line, whose options take precedence (handy for a test). See
//! `config.example.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qdw_video::encode::{Codec, Settings};
use serde::Deserialize;

use crate::providers::{self, Providers, Source};
use crate::{session, web};

pub const USAGE: &str = "usage: qemu-dbus-webrtc [--config <file.toml>] \
                         [--qmp <QMP socket> | --dbus <D-Bus address> | --domain <libvirt domain>]... \
                         [--codec av1|vp9|hevc] [--chroma 420|444] [--software] [--bitrate <max kbit/s>] \
                         [--record <path prefix>] [--listen <address:port>] \
                         --tls <certificate.pem> <key.pem>\n       \
                         qemu-dbus-webrtc --codecs   (what the GPUs encode)";

/// Where the viewer pages are served by default: this host only, since there is no
/// authentication yet.
const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

/// The bandwidth estimation, in kbit/s, by default: never below the minimum (a desktop below
/// 4 Mbit/s is not usable, so the session rather fails visibly), starting high (sharp at once
/// on today's connections, where a ramp from the floor would show a blurred screen for
/// seconds), up to the maximum.
const MIN_BITRATE: u32 = 4_000;
const START_BITRATE: u32 = 10_000;
const MAX_BITRATE: u32 = 30_000;

/// Where libvirt puts the D-Bus bus of a domain's display.
const LIBVIRT_DBUS_DIR: &str = "/run/libvirt/qemu/dbus";
/// Where Proxmox VE puts the VMs' QMP sockets and their configurations.
const PROXMOX_RUN_DIR: &str = "/var/run/qemu-server";
const PROXMOX_CONF_DIR: &str = "/etc/pve/qemu-server";

/// The configuration file. Every setting is optional; an unknown one is an error, so that a
/// typing mistake does not go unnoticed.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct File {
    auth: AuthFile,
    listen: Listen,
    video: Video,
    bitrate: Bitrate,
    libvirt: Libvirt,
    proxmox: ProxmoxFile,
    vm: Vec<Vm>,
}

/// Who may open a VM's console.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthFile {
    /// "password" (the default): the server's user and password; "none": anyone who reaches
    /// the server.
    method: Option<String>,
    user: Option<String>,
    /// Its Argon2id hash: `qemu-dbus-webrtc hash-password`.
    password: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Listen {
    address: Option<SocketAddr>,
    certificate: Option<PathBuf>,
    key: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Video {
    codec: Option<String>,
    chroma: Option<String>,
    software: Option<bool>,
    record: Option<String>,
}

/// In kbit/s.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Bitrate {
    min: Option<u32>,
    start: Option<u32>,
    max: Option<u32>,
}

/// The VMs of libvirt: on by default.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Libvirt {
    enabled: Option<bool>,
    dir: Option<PathBuf>,
    /// Start a domain that is off when a client asks for it: off by default.
    start: Option<bool>,
}

/// The VMs of Proxmox VE: off by default.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProxmoxFile {
    enabled: Option<bool>,
    run_dir: Option<PathBuf>,
    conf_dir: Option<PathBuf>,
}

/// A VM of no VM manager the server knows: one of the three.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Vm {
    qmp: Option<PathBuf>,
    dbus: Option<String>,
    domain: Option<String>,
}

/// How every VM is encoded.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub settings: Settings,
    /// A codec was asked for: no other codec in its place.
    pub codec_given: bool,
    pub bitrates: session::Bitrates,
    /// Record each console's stream to `<prefix>-<vm uuid>-<console>.<extension>`.
    pub record: Option<String>,
}

/// Who may open a VM's console.
#[derive(Debug, Clone)]
pub enum Auth {
    /// The server's user and password (see `access.rs`).
    Password(crate::access::Password),
    /// Anyone who reaches the server: for a test, on a trusted network.
    None,
}

impl Config {
    /// Who may open a VM's console.
    pub fn auth(&self) -> Result<Auth> {
        self.auth.clone().context(
            "[auth] takes a user and its password's hash (`qemu-dbus-webrtc hash-password`), \
             or method = \"none\" for a test",
        )
    }

    /// The certificate to serve with.
    pub fn tls(&self) -> Result<web::Tls> {
        // Plain HTTP would carry the session in clear.
        self.tls.clone().context(
            "a certificate and its key are required, the pages being served in HTTPS only: \
             certificate and key in [listen], or --tls <certificate.pem> <key.pem>",
        )
    }
}

pub struct Config {
    /// Who may open a VM's console: required to serve (see `Config::auth`).
    auth: Option<Auth>,
    pub vm: VmConfig,
    /// The address of the viewer pages and their signalling.
    pub listen: SocketAddr,
    /// Their certificate, always in HTTPS: required to serve (see `Config::tls`), not by the
    /// tools.
    tls: Option<web::Tls>,
    /// Where the VMs are found.
    pub providers: Providers,
}

/// The settings from the configuration file, if `--config` names one, then from the other
/// options.
pub fn parse_args() -> Result<Config> {
    parse(std::env::args().skip(1).collect())
}

pub fn parse(args: Vec<String>) -> Result<Config> {
    // The file first, wherever --config is on the line: the options then override it.
    let mut file = File::default();
    if let Some(i) = args.iter().position(|a| a == "--config") {
        let Some(path) = args.get(i + 1) else {
            bail!(USAGE)
        };
        file = read(Path::new(path))?;
    }
    let mut listen = file.listen;
    let mut video = file.video;
    let bitrate = file.bitrate;
    let mut max_bitrate = bitrate.max;
    let mut vms = file.vm;

    let mut args = args.into_iter();
    while let Some(option) = args.next() {
        // Options without a value.
        if option == "--software" {
            video.software = Some(true);
            continue;
        }
        let Some(value) = args.next() else {
            bail!(USAGE)
        };
        match option.as_str() {
            "--config" => {}
            "--qmp" => vms.push(Vm {
                qmp: Some(value.into()),
                ..Vm::default()
            }),
            "--dbus" => vms.push(Vm {
                dbus: Some(value),
                ..Vm::default()
            }),
            "--domain" => vms.push(Vm {
                domain: Some(value),
                ..Vm::default()
            }),
            "--codec" => video.codec = Some(value),
            "--chroma" => video.chroma = Some(value),
            "--bitrate" => max_bitrate = Some(value.parse().context(USAGE)?),
            "--record" => video.record = Some(value),
            "--listen" => listen.address = Some(value.parse().context(USAGE)?),
            "--tls" => {
                let Some(key) = args.next() else { bail!(USAGE) };
                listen.certificate = Some(value.into());
                listen.key = Some(key.into());
            }
            _ => bail!(USAGE),
        }
    }

    let libvirt_dir = file.libvirt.dir.unwrap_or_else(|| LIBVIRT_DBUS_DIR.into());
    let statics = vms
        .into_iter()
        .map(|vm| match (vm.qmp, vm.dbus, vm.domain) {
            (Some(qmp), None, None) => Ok(Source::Qmp(qmp)),
            (None, Some(address), None) => Ok(Source::Dbus(address)),
            (None, None, Some(domain)) => Ok(Source::Domain {
                dir: libvirt_dir.clone(),
                domain,
            }),
            _ => bail!("a [[vm]] takes one of qmp, dbus or domain"),
        })
        .collect::<Result<Vec<_>>>()?;
    let providers = Providers::new(
        file.libvirt.enabled.unwrap_or(true).then(|| {
            crate::libvirt::Libvirt::new(libvirt_dir, file.libvirt.start.unwrap_or(false))
        }),
        file.proxmox
            .enabled
            .unwrap_or(false)
            .then(|| providers::Proxmox {
                run_dir: file
                    .proxmox
                    .run_dir
                    .unwrap_or_else(|| PROXMOX_RUN_DIR.into()),
                conf_dir: file
                    .proxmox
                    .conf_dir
                    .unwrap_or_else(|| PROXMOX_CONF_DIR.into()),
            }),
        statics,
    );

    let codec_given = video.codec.is_some();
    let software = video.software.unwrap_or(false);
    let codec = match &video.codec {
        Some(name) => {
            Codec::parse(name).with_context(|| format!("codec {name}: av1, vp9 or hevc"))?
        }
        // On the CPU, VP9.
        None if software => Codec::Vp9,
        None => Codec::Av1,
    };
    let full_chroma = match video.chroma.as_deref() {
        None | Some("420") => false,
        Some("444") => true,
        Some(other) => bail!("chroma {other}: 420 or 444"),
    };
    if full_chroma && !codec.has_full_chroma() {
        bail!("{codec:?} has no 4:4:4 on the hardware encoders");
    }

    let min = bitrate.min.unwrap_or(MIN_BITRATE);
    let max = max_bitrate.unwrap_or(MAX_BITRATE).max(min);
    let start = bitrate.start.unwrap_or(START_BITRATE).clamp(min, max);
    let bitrates = session::Bitrates { min, start, max };

    let tls = match (listen.certificate, listen.key) {
        (Some(certificate), Some(key)) => Some(web::Tls { certificate, key }),
        (None, None) => None,
        _ => bail!("HTTPS takes a certificate and its key"),
    };
    let listen = match listen.address {
        Some(address) => address,
        None => DEFAULT_LISTEN.parse()?,
    };

    let auth = match file.auth.method.as_deref() {
        None | Some("password") => match (file.auth.user, file.auth.password) {
            (Some(user), Some(hash)) => Some(Auth::Password(
                crate::access::Password::new(user, hash).context("in [auth]")?,
            )),
            (None, None) => None,
            _ => bail!("[auth] takes both a user and its password's hash"),
        },
        Some("none") => Some(Auth::None),
        Some(other) => bail!("[auth] method {other}: password or none"),
    };

    Ok(Config {
        auth,
        vm: VmConfig {
            settings: Settings {
                codec,
                full_chroma,
                bitrate: start,
                ten_bits: false,
                software,
            },
            codec_given,
            bitrates,
            record: video.record,
        },
        listen,
        tls,
        providers,
    })
}

fn read(path: &Path) -> Result<File> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the configuration {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("in the configuration {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn a_file_with_every_setting() {
        let file: File = toml::from_str(include_str!("../../config.example.toml")).unwrap();
        assert_eq!(file.listen.address, Some("0.0.0.0:8090".parse().unwrap()));
        assert_eq!(file.video.codec.as_deref(), Some("hevc"));
        assert_eq!(file.bitrate.max, Some(30_000));
        assert_eq!(file.libvirt.enabled, Some(true));
        assert_eq!(file.vm.len(), 1);
    }

    #[test]
    fn unknown_settings_are_refused() {
        assert!(toml::from_str::<File>("[video]\ncodecs = \"av1\"").is_err());
        assert!(toml::from_str::<File>("[videos]").is_err());
        assert!(toml::from_str::<File>("[[vm]]\nqmps = \"/q\"").is_err());
    }

    #[test]
    fn options_override_the_file() {
        let dir = std::env::temp_dir().join(format!("qdw-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.toml");
        std::fs::write(
            &path,
            "[listen]\ncertificate = \"c.pem\"\nkey = \"k.pem\"\n[video]\ncodec = \"hevc\"\n\
             [bitrate]\nmax = 20000\n[libvirt]\nenabled = false\n[[vm]]\nqmp = \"/run/vm.qmp\"\n",
        )
        .unwrap();
        let config = path.to_str().unwrap();
        let c = parse(args(&format!(
            "--config {config} --codec av1 --dbus unix:path=/x"
        )))
        .unwrap();
        assert_eq!(c.vm.settings.codec, Codec::Av1);
        assert_eq!(c.vm.bitrates.max, 20_000);
        assert!(c.providers.libvirt.is_none());
        assert_eq!(c.providers.statics.0.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn https_only() {
        let error = parse(args("--qmp /q")).unwrap().tls().err().unwrap();
        assert!(error.to_string().contains("HTTPS only"), "{error}");
    }

    #[test]
    fn libvirt_by_default_proxmox_on_demand() {
        let c = parse(args("--tls c.pem k.pem")).unwrap();
        assert!(c.providers.libvirt.is_some());
        assert!(c.providers.proxmox.is_none());
        assert!(c.providers.statics.0.is_empty());
    }

    #[test]
    fn bitrates_stay_in_order() {
        let c = parse(args("--tls c.pem k.pem --bitrate 2000")).unwrap();
        assert_eq!(
            (c.vm.bitrates.min, c.vm.bitrates.start, c.vm.bitrates.max),
            (4000, 4000, 4000)
        );
        let c = parse(args("--tls c.pem k.pem")).unwrap();
        assert_eq!(
            (c.vm.bitrates.min, c.vm.bitrates.start, c.vm.bitrates.max),
            (4000, 10_000, 30_000)
        );
    }
}
