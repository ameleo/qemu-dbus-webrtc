// SPDX-License-Identifier: MPL-2.0

//! The server's settings: from a configuration file (`--config`, TOML), then from the command
//! line, whose options take precedence (handy for a test). See `config.example.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qdw_qemu::display::Attach;
use qdw_video::encode::{Codec, Settings};
use serde::Deserialize;

use crate::{session, web};

pub const USAGE: &str = "usage: qemu-dbus-webrtc [--config <file.toml>] \
                         (--qmp <QMP socket> | --dbus <D-Bus address> | --domain <libvirt domain>) \
                         [--codec av1|vp9|hevc] [--chroma 420|444] [--software] [--bitrate <max kbit/s>] \
                         [--record <path prefix>] [--listen <address:port>] \
                         [--tls <certificate.pem> <key.pem>]\n       \
                         qemu-dbus-webrtc --codecs   (what the GPUs encode)";

/// Where the viewer page is served by default: this host only, since there is no
/// authentication yet.
const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

/// The bandwidth estimation, in kbit/s, by default: never below the minimum (a desktop below
/// 4 Mbit/s is not usable, so the session rather fails visibly), starting high (sharp at once
/// on today's connections, where a ramp from the floor would show a blurred screen for
/// seconds), up to the maximum.
const MIN_BITRATE: u32 = 4_000;
const START_BITRATE: u32 = 10_000;
const MAX_BITRATE: u32 = 30_000;

/// Where libvirt puts the D-Bus bus of a domain's display: `<id>-<domain>-dbus.sock`, the id
/// changing at each start of the domain.
const LIBVIRT_DBUS_DIR: &str = "/run/libvirt/qemu/dbus";

/// The configuration file. Every setting is optional; an unknown one is an error, so that a
/// typing mistake does not go unnoticed.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct File {
    vm: Vm,
    listen: Listen,
    video: Video,
    bitrate: Bitrate,
}

/// The VM to attach to: one of the three.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Vm {
    /// The VM's QMP socket: QEMU hands the display over a peer-to-peer connection.
    qmp: Option<PathBuf>,
    /// The D-Bus bus on which QEMU owns `org.qemu` (libvirt's `<graphics type='dbus'>`).
    dbus: Option<String>,
    /// A libvirt domain: its display's bus, found by the domain's name.
    domain: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Listen {
    address: Option<SocketAddr>,
    /// HTTPS, with both.
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

pub struct Args {
    pub attach: Attach,
    pub settings: Settings,
    /// A codec was asked for: no other codec in its place.
    pub codec_given: bool,
    pub bitrates: session::Bitrates,
    /// Record each console's stream to `<prefix>-<console>.<extension>`.
    pub record: Option<String>,
    /// The address of the viewer page and its signalling.
    pub listen: SocketAddr,
    /// Serve in HTTPS.
    pub tls: Option<web::Tls>,
}

/// The settings from the configuration file, if `--config` names one, then from the other
/// options.
pub fn parse_args() -> Result<Args> {
    parse(std::env::args().skip(1).collect())
}

fn parse(args: Vec<String>) -> Result<Args> {
    // The file first, wherever --config is on the line: the options then override it.
    let mut file = File::default();
    if let Some(i) = args.iter().position(|a| a == "--config") {
        let Some(path) = args.get(i + 1) else {
            bail!(USAGE)
        };
        file = read(Path::new(path))?;
    }
    let mut vm = file.vm;
    let mut listen = file.listen;
    let mut video = file.video;
    let bitrate = file.bitrate;
    let mut max_bitrate = bitrate.max;

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
        // The VM given on the command line replaces the file's.
        let mut set_vm = |qmp, dbus, domain| vm = Vm { qmp, dbus, domain };
        match option.as_str() {
            "--config" => {}
            "--qmp" => set_vm(Some(value.into()), None, None),
            "--dbus" => set_vm(None, Some(value), None),
            "--domain" => set_vm(None, None, Some(value)),
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

    let attach = match (vm.qmp, vm.dbus, vm.domain) {
        (Some(qmp), None, None) => Attach::P2p { qmp },
        (None, Some(address), None) => Attach::Bus { address },
        (None, None, Some(domain)) => Attach::Bus {
            address: libvirt_bus(Path::new(LIBVIRT_DBUS_DIR), &domain)?,
        },
        (None, None, None) => bail!("which VM? qmp, dbus or domain\n{USAGE}"),
        _ => bail!("one VM only: qmp, dbus or domain"),
    };

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
        _ => bail!("HTTPS needs both a certificate and a key"),
    };
    let listen = match listen.address {
        Some(address) => address,
        None => DEFAULT_LISTEN.parse()?,
    };

    Ok(Args {
        attach,
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
        listen,
        tls,
    })
}

fn read(path: &Path) -> Result<File> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the configuration {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("in the configuration {}", path.display()))
}

/// The D-Bus address of a libvirt domain's display: its socket in `dir`, whatever id the
/// domain has now.
fn libvirt_bus(dir: &Path, domain: &str) -> Result<String> {
    let suffix = format!("-{domain}-dbus.sock");
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("looking for {domain} in {}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(&suffix) else {
            continue;
        };
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(format!("unix:path={}", entry.path().display()));
        }
    }
    bail!(
        "no D-Bus display for the libvirt domain {domain}: is it running, with <graphics type='dbus'>?"
    )
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
    }

    #[test]
    fn unknown_settings_are_refused() {
        assert!(toml::from_str::<File>("[video]\ncodecs = \"av1\"").is_err());
        assert!(toml::from_str::<File>("[videos]").is_err());
    }

    #[test]
    fn options_override_the_file() {
        let dir = std::env::temp_dir().join(format!("qdw-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vm.toml");
        std::fs::write(
            &path,
            "[vm]\nqmp = \"/run/vm.qmp\"\n[video]\ncodec = \"hevc\"\n[bitrate]\nmax = 20000\n",
        )
        .unwrap();
        let config = path.to_str().unwrap();
        let a = parse(args(&format!("--config {config} --codec av1"))).unwrap();
        assert_eq!(a.settings.codec, Codec::Av1);
        assert_eq!(a.bitrates.max, 20_000);
        assert!(matches!(a.attach, Attach::P2p { .. }));
        let a = parse(args(&format!("--dbus unix:path=/x --config {config}"))).unwrap();
        assert!(matches!(a.attach, Attach::Bus { .. }));
        assert_eq!(a.settings.codec, Codec::Hevc);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bitrates_stay_in_order() {
        let a = parse(args("--qmp /q --bitrate 2000")).unwrap();
        assert_eq!(
            (a.bitrates.min, a.bitrates.start, a.bitrates.max),
            (4000, 4000, 4000)
        );
        let a = parse(args("--qmp /q")).unwrap();
        assert_eq!(
            (a.bitrates.min, a.bitrates.start, a.bitrates.max),
            (4000, 10_000, 30_000)
        );
    }

    #[test]
    fn a_domain_by_its_name() {
        let dir = std::env::temp_dir().join(format!("qdw-libvirt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "6-win-dbus-dbus.sock",
            "6-win-dbus-dbus.pid",
            "7-other-win-dbus-dbus.sock",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let address = libvirt_bus(&dir, "win-dbus").unwrap();
        assert!(address.ends_with("/6-win-dbus-dbus.sock"), "{address}");
        assert!(libvirt_bus(&dir, "missing").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
