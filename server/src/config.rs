// SPDX-License-Identifier: MPL-2.0

//! The server's settings: from a configuration file (`--config`, TOML), one per host, then from
//! the command line, whose options take precedence (handy for a test). See
//! `config.example.toml`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use qdw_video::encode::{Codec, Settings};
use serde::Deserialize;

use crate::network::turn;
use crate::session;
use crate::session::transport;
use crate::vms::providers::{Providers, Source};

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

/// The STUN server by default, as WebRTC clients commonly use: without one, a client outside
/// the host's network, behind a NAT, reaches no session unless the router forwards ports to it.
const DEFAULT_STUN: &str = "stun.l.google.com:19302";

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
    /// Removed: its own ACME client gave the server its certificate. Named, for a clear error.
    acme: Option<serde::de::IgnoredAny>,
    auth: AuthFile,
    listen: Listen,
    video: Video,
    bitrate: Bitrate,
    libvirt: Libvirt,
    proxmox: ProxmoxFile,
    vm: Vec<Vm>,
    account: Vec<AccountFile>,
    turn: Vec<TurnFile>,
    broker: Option<BrokerFile>,
}

/// The connection broker the host reaches (see `broker`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrokerFile {
    url: String,
    host_id: String,
    /// The roots that authenticate the broker, instead of the system's.
    ca_bundle: Option<PathBuf>,
    /// The broker's ACME directory, for the host's certificate to it, with which it
    /// authenticates (`qemu-dbus-webrtc enroll`).
    acme_directory: String,
    /// The TPM that holds the host's key, its TCTI: "device:/dev/tpmrm0" by default; "none"
    /// for a host the broker enrolled without a TPM.
    tpm: Option<String>,
    /// Where the host keeps its ACME account, keys and certificate.
    state_dir: Option<PathBuf>,
}

/// A TURN server the clients may relay their media through (see `turn`): its URIs, and either
/// the secret it shares with the server, or fixed credentials. Its `Debug` shows neither.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnFile {
    urls: Vec<String>,
    secret: Option<String>,
    username: Option<String>,
    credential: Option<String>,
}

impl std::fmt::Debug for TurnFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnFile")
            .field("urls", &self.urls)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl TurnFile {
    fn server(self) -> Result<turn::Server> {
        if self.urls.is_empty() {
            bail!("no urls");
        }
        for url in &self.urls {
            turn::check_url(url)?;
        }
        let credentials = match (self.secret, self.username, self.credential) {
            (Some(secret), None, None) if !secret.is_empty() => turn::Credentials::Secret(secret),
            (None, Some(username), Some(credential))
                if !username.is_empty() && !credential.is_empty() =>
            {
                turn::Credentials::Fixed {
                    username,
                    credential,
                }
            }
            _ => bail!("either a secret, or a username and a credential"),
        };
        Ok(turn::Server {
            urls: self.urls,
            credentials,
        })
    }
}

/// An account: its user, its password, and the VMs it may open.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountFile {
    user: String,
    /// Its Argon2id hash: `qemu-dbus-webrtc hash-password`.
    password: String,
    /// The VMs it may open, each by its UUID or its name; "*" for all of them.
    vms: Vec<String>,
    /// It may reset them, from the console.
    #[serde(default)]
    power: bool,
    /// It may watch their sessions, and ask to control them, with the guest's user's consent.
    #[serde(default)]
    shadow: bool,
}

/// Who may open a VM's console.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthFile {
    /// "password" (the default): the accounts' users and passwords; "none": anyone who
    /// reaches the server.
    method: Option<String>,
    /// An account that may open every VM, besides those of `[[account]]`.
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
    /// The address clients reach the host at from outside (see `transport::Network`).
    public_address: Option<String>,
    /// The UDP ports of the sessions, "first-last".
    udp_ports: Option<String>,
    /// One UDP port for every session, instead.
    udp_port: Option<u16>,
    /// A STUN server, "host:port" (see `transport::Network`): `DEFAULT_STUN` when absent, none
    /// when empty.
    stun: Option<String>,
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
    pub bitrates: transport::Bitrates,
    /// Record each console's stream to `<prefix>-<vm uuid>-<console>.<extension>`.
    pub record: Option<String>,
    /// How clients reach the host from outside.
    pub network: transport::Network,
}

/// Who may open a VM's console.
#[derive(Debug, Clone)]
pub enum Auth {
    /// The accounts, and the VMs each may open (see `access.rs`).
    Password(crate::host::access::Accounts),
    /// Anyone who reaches the server: for a test, on a trusted network.
    None,
}

impl Config {
    /// Who may open a VM's console.
    pub fn auth(&self) -> Result<Auth> {
        self.auth.clone().context(
            "no account: [[account]] entries, each with a user, its password's hash \
             (`qemu-dbus-webrtc hash-password`) and its VMs, or method = \"none\" in [auth] for \
             a test",
        )
    }

    /// Whether the host serves its own pages: with a broker, only when it has both accounts and
    /// a certificate; without one, always (and `auth` and `tls` say what is missing).
    /// Whether the host serves its own pages: never with a broker, which serves them.
    pub fn serves_pages(&self) -> bool {
        self.broker.is_none()
    }

    /// The certificate to serve with.
    pub fn tls(&self) -> Result<session::Tls> {
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
    tls: Option<session::Tls>,
    /// Where the VMs are found.
    pub providers: Providers,
    /// The connection broker the host reaches, if it has one.
    pub broker: Option<crate::host::broker::Settings>,
    /// How the host gets its certificate from the broker's ACME server, if it does.
    pub attestation: Option<crate::host::attestation::Settings>,
}

/// The settings from the configuration file, if `--config` names one, then from the other
/// options.
/// The host's TPM by default: the kernel's resource manager.
const DEFAULT_TPM: &str = "device:/dev/tpmrm0";
/// Where the host keeps its certificate to the broker by default.
const DEFAULT_STATE_DIR: &str = "/var/lib/qemu-dbus-webrtc/broker";

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
    if file.acme.is_some() {
        bail!(
            "[acme] is no longer supported: get the certificate with an ACME client of your own \
             (certbot, step...) and give its files in [listen] certificate and key"
        );
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
            crate::vms::libvirt::Libvirt::new(libvirt_dir, file.libvirt.start.unwrap_or(false))
        }),
        file.proxmox
            .enabled
            .unwrap_or(false)
            .then(|| crate::vms::proxmox::Proxmox {
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
    // The bitrate follows the estimate by its relative changes: from 0, it never would.
    if min == 0 {
        bail!("[bitrate] min must be above 0 kbit/s");
    }
    let max = max_bitrate.unwrap_or(MAX_BITRATE).max(min);
    let start = bitrate.start.unwrap_or(START_BITRATE).clamp(min, max);
    let bitrates = transport::Bitrates { min, start, max };

    // A host with a broker serves its VMs through it only: the broker is the deployment's one
    // entry point (its bastion). Nothing that opens the host's own pages, nor a relay of the
    // host's own beside the broker's.
    if file.broker.is_some() {
        let mut direct = Vec::new();
        if listen.address.is_some() {
            direct.push("[listen] address (--listen)");
        }
        if listen.certificate.is_some() || listen.key.is_some() {
            direct.push("[listen] certificate and key (--tls)");
        }
        if file.auth.method.is_some() || file.auth.user.is_some() || file.auth.password.is_some() {
            direct.push("[auth]");
        }
        if !file.account.is_empty() {
            direct.push("[[account]]");
        }
        if !file.turn.is_empty() {
            direct.push("[[turn]]");
        }
        if !direct.is_empty() {
            bail!(
                "with [broker], the host serves its VMs through the broker only: remove {}",
                direct.join(", ")
            );
        }
    }

    let tls = match (listen.certificate, listen.key) {
        (Some(certificate), Some(key)) => Some(session::Tls { certificate, key }),
        (None, None) => None,
        _ => bail!("HTTPS takes a certificate and its key"),
    };
    let network = transport::Network {
        public_address: match listen.public_address {
            Some(name) if name.trim().is_empty() => bail!("[listen] public_address is empty"),
            name => name,
        },
        udp_ports: listen
            .udp_ports
            .as_deref()
            .map(udp_ports)
            .transpose()
            .context("in [listen] udp_ports")?,
        udp_port: match (listen.udp_port, &listen.udp_ports) {
            (Some(_), Some(_)) => {
                bail!("[listen]: udp_port (one port for every session) or udp_ports, not both")
            }
            (Some(port), None) => {
                Some(crate::network::mux::check_port(port).context("in [listen]")?)
            }
            (None, _) => None,
        },
        stun: match listen.stun.as_deref().map(str::trim) {
            None => Some(DEFAULT_STUN.to_owned()),
            Some("") => None,
            Some(server) if !server.contains(':') => {
                bail!("[listen] stun: \"host:port\", such as \"stun.example.com:3478\", or \"\"")
            }
            Some(server) => Some(server.to_owned()),
        },
        turn: file
            .turn
            .into_iter()
            .enumerate()
            .map(|(i, turn)| {
                turn.server()
                    .with_context(|| format!("in [[turn]] {}", i + 1))
            })
            .collect::<Result<_>>()?,
    };
    let listen = match listen.address {
        Some(address) => address,
        None => DEFAULT_LISTEN.parse()?,
    };

    let auth = match file.auth.method.as_deref() {
        None | Some("password") => {
            use crate::host::access::{Account, Accounts, Grant};
            let mut accounts = Vec::new();
            // [auth]'s user: every VM, as before accounts had their VMs.
            match (file.auth.user, file.auth.password) {
                (Some(user), Some(hash)) => {
                    accounts.push(Account::new(user, hash, Grant::All).context("in [auth]")?)
                }
                (None, None) => {}
                _ => bail!("[auth] takes both a user and its password's hash"),
            }
            for account in file.account {
                let user = account.user.clone();
                let grant = Grant::new(account.vms)
                    .with_context(|| format!("in the [[account]] of {user}"))?;
                let mut new = Account::new(account.user, account.password, grant)
                    .with_context(|| format!("in the [[account]] of {user}"))?;
                new.power = account.power;
                new.shadow = account.shadow;
                accounts.push(new);
            }
            if accounts.is_empty() {
                None
            } else {
                Some(Auth::Password(Accounts::new(accounts)?))
            }
        }
        Some("none") if file.account.is_empty() => Some(Auth::None),
        Some("none") => bail!("[auth] method none lets anyone in: no [[account]] with it"),
        Some(other) => bail!("[auth] method {other}: password or none"),
    };

    let (broker, attestation) = match file.broker {
        None => (None, None),
        Some(b) => {
            let directory = &b.acme_directory;
            if !directory.starts_with("https://") {
                bail!("[broker] acme_directory: {directory}: not https://");
            }
            let roots = match &b.ca_bundle {
                Some(path) => Some(
                    std::fs::read(path)
                        .with_context(|| format!("[broker] ca_bundle {}", path.display()))?,
                ),
                None => None,
            };
            let tpm = match b.tpm.as_deref() {
                None => Some(DEFAULT_TPM.to_owned()),
                Some("none") => None,
                Some(tcti) => Some(tcti.to_owned()),
            };
            let attestation = crate::host::attestation::Settings {
                directory: directory.clone(),
                host_id: b.host_id.clone(),
                roots,
                state: b
                    .state_dir
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
                tpm,
                enrollment: None,
            };
            let broker =
                crate::host::broker::settings(b.url, b.host_id, b.ca_bundle, attestation.clone())
                    .context("in [broker]")?;
            (Some(broker), Some(attestation))
        }
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
                max_bitrate: max,
            },
            codec_given,
            bitrates,
            record: video.record,
            network,
        },
        listen,
        tls,
        providers,
        broker,
        attestation,
    })
}

/// A range of UDP ports, "first-last" (or one port): its bounds.
fn udp_ports(text: &str) -> Result<(u16, u16)> {
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let port = |p: &str| -> Result<u16> {
        p.trim()
            .parse()
            .with_context(|| format!("{text}: ports \"first-last\", 1 to 65535"))
    };
    let (first, last) = (port(first)?, port(last)?);
    if first == 0 || first > last {
        bail!("{text}: ports \"first-last\", 1 to 65535, the first no larger than the last");
    }
    Ok((first, last))
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
        assert_eq!(file.account.len(), 1);
        assert_eq!(file.account[0].vms.len(), 2);
    }

    /// [auth]'s user opens every VM; each [[account]] its own; mistakes fail the start.
    #[test]
    fn accounts() {
        use crate::host::access::{Grant, hash};
        let dir = std::env::temp_dir().join(format!("qdw-config-accounts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.toml");
        let (admin, alice) = (hash("root").unwrap(), hash("secret").unwrap());
        let parse_with = |text: &str| {
            std::fs::write(&path, text).unwrap();
            parse(vec!["--config".into(), path.display().to_string()])
        };
        let config = parse_with(&format!(
            "[auth]\nuser = \"admin\"\npassword = \"{admin}\"\n\
             [[account]]\nuser = \"alice\"\npassword = \"{alice}\"\nvms = [\"win11\"]\n\
             power = true\nshadow = true\n"
        ))
        .unwrap();
        let Auth::Password(accounts) = config.auth().unwrap() else {
            panic!("accounts expected")
        };
        assert_eq!(accounts.check("admin", "root").unwrap().grant, Grant::All);
        assert_eq!(
            accounts.check("alice", "secret").unwrap().grant,
            Grant::Only(vec!["win11".into()])
        );
        // The power actions only for the account that says so.
        assert!(accounts.check("alice", "secret").unwrap().power);
        assert!(!accounts.check("admin", "root").unwrap().power);
        // Watching too.
        assert!(accounts.check("alice", "secret").unwrap().shadow);
        assert!(!accounts.check("admin", "root").unwrap().shadow);
        // Accounts alone, without [auth]'s user.
        let alone =
            format!("[[account]]\nuser = \"alice\"\npassword = \"{alice}\"\nvms = [\"*\"]\n");
        assert!(matches!(
            parse_with(&alone).unwrap().auth(),
            Ok(Auth::Password(_))
        ));
        // No VM, the same user twice, anyone let in beside accounts: refused.
        let no_vm = format!("[[account]]\nuser = \"alice\"\npassword = \"{alice}\"\nvms = []\n");
        assert!(parse_with(&no_vm).is_err());
        assert!(parse_with(&format!("{alone}{alone}")).is_err());
        assert!(parse_with(&format!("[auth]\nmethod = \"none\"\n{alone}")).is_err());
        // No account at all: nothing to serve with.
        assert!(parse_with("").unwrap().auth().is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// STUN by default, none when empty, another when named.
    #[test]
    fn stun_servers() {
        let stun = |text: &str| parse_file_listen(text).map(|c| c.vm.network.stun);
        assert_eq!(stun("").unwrap().as_deref(), Some(DEFAULT_STUN));
        assert_eq!(stun("stun = \"\"\n").unwrap(), None);
        assert_eq!(
            stun("stun = \"stun.example.com:3478\"\n")
                .unwrap()
                .as_deref(),
            Some("stun.example.com:3478")
        );
        assert!(stun("stun = \"stun.example.com\"\n").is_err());
    }

    /// TURN servers: a secret or fixed credentials, never both, and checked URIs.
    #[test]
    fn turn_servers() {
        let turn = |text: &str| parse_file_listen(&format!("\n{text}")).map(|c| c.vm.network.turn);
        assert!(turn("").unwrap().is_empty());
        let servers = turn(
            "[[turn]]\nurls = [\"turns:relay.example.com:443?transport=tcp\"]\nsecret = \"s\"\n\
             [[turn]]\nurls = [\"turn:192.0.2.1\"]\nusername = \"u\"\ncredential = \"c\"\n",
        )
        .unwrap();
        assert_eq!(
            servers[0].credentials,
            turn::Credentials::Secret("s".into())
        );
        assert_eq!(
            servers[1].credentials,
            turn::Credentials::Fixed {
                username: "u".into(),
                credential: "c".into()
            }
        );
        // Both ways, neither, an empty secret, no URI, a wrong URI.
        for wrong in [
            "[[turn]]\nurls = [\"turn:a\"]\nsecret = \"s\"\nusername = \"u\"\ncredential = \"c\"\n",
            "[[turn]]\nurls = [\"turn:a\"]\n",
            "[[turn]]\nurls = [\"turn:a\"]\nsecret = \"\"\n",
            "[[turn]]\nurls = [\"turn:a\"]\nusername = \"u\"\ncredential = \"\"\n",
            "[[turn]]\nurls = []\nsecret = \"s\"\n",
            "[[turn]]\nurls = [\"stun:a\"]\nsecret = \"s\"\n",
        ] {
            assert!(turn(wrong).is_err(), "{wrong}");
        }
    }

    /// A configuration file with `listen`, the [listen] section's settings, parsed.
    fn parse_file_listen(listen: &str) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!(
            "qdw-config-listen-{}-{}",
            std::process::id(),
            listen.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.toml");
        std::fs::write(&path, format!("[listen]\n{listen}")).unwrap();
        let config = parse(vec!["--config".into(), path.display().to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
        config
    }

    #[test]
    fn udp_port_ranges() {
        assert_eq!(udp_ports("40000-40099").unwrap(), (40000, 40099));
        assert_eq!(udp_ports(" 40000 - 40000 ").unwrap(), (40000, 40000));
        assert_eq!(udp_ports("50000").unwrap(), (50000, 50000));
        assert!(udp_ports("40099-40000").is_err());
        assert!(udp_ports("0-10").is_err());
        assert!(udp_ports("40000-70000").is_err());
        assert!(udp_ports("udp").is_err());
    }

    /// One UDP port for every session: a port, alone.
    #[test]
    fn one_udp_port() {
        let config = parse_file_listen("udp_port = 3478\n").unwrap();
        assert_eq!(config.vm.network.udp_port, Some(3478));
        assert!(parse_file_listen("udp_port = 0\n").is_err());
        let both = parse_file_listen("udp_port = 3478\nudp_ports = \"40000-40099\"\n");
        assert!(format!("{:#}", both.err().expect("refused")).contains("not both"));
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
    fn a_bitrate_of_zero_is_refused() {
        let dir = std::env::temp_dir().join(format!("qdw-config-zero-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.toml");
        std::fs::write(&path, "[bitrate]\nmin = 0\n").unwrap();
        let error = parse(args(&format!("--config {}", path.to_str().unwrap())))
            .err()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.to_string().contains("above 0"), "{error}");
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

    /// `[acme]` is gone: a configuration that still has it is told what to do instead.
    #[test]
    fn acme_is_gone() {
        let dir = std::env::temp_dir().join(format!("qdw-config-acme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[acme]\ndirectory = \"https://ca/d\"\ndomains = [\"h\"]\n",
        )
        .unwrap();
        let error = format!(
            "{:#}",
            parse(args(&format!("--config {}", path.display())))
                .err()
                .expect("refused")
        );
        assert!(error.contains("[acme] is no longer supported"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// With [broker], the host serves through the broker only: whatever would open its own
    /// pages, or add a relay of its own, is refused; the media's own settings stay.
    #[test]
    fn a_broker_or_direct_access_not_both() {
        let dir = std::env::temp_dir().join(format!("qdw-config-broker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let broker = "[broker]\nurl = \"https://broker:7443\"\nhost_id = \"host-1\"\n\
                      acme_directory = \"https://broker:7443/acme/directory\"\n\
                      tpm = \"none\"\n";
        let parsed = |text: &str, more: &str| {
            std::fs::write(&path, text).unwrap();
            parse(args(&format!("--config {} {more}", path.display())))
        };
        let only = parsed(
            &format!(
                "[listen]\npublic_address = \"203.0.113.1\"\nudp_ports = \"40000-40099\"\n{broker}"
            ),
            "",
        )
        .unwrap();
        assert!(only.broker.is_some() && !only.serves_pages());
        for (text, more, named) in [
            (
                format!("[listen]\naddress = \"0.0.0.0:8090\"\n{broker}"),
                "",
                "[listen] address",
            ),
            (
                broker.to_owned(),
                "--listen 0.0.0.0:8090",
                "[listen] address",
            ),
            (
                broker.to_owned(),
                "--tls c.pem k.pem",
                "certificate and key",
            ),
            (format!("[auth]\nmethod = \"none\"\n{broker}"), "", "[auth]"),
            (
                format!("[[account]]\nuser = \"a\"\npassword = \"x\"\nvms = [\"*\"]\n{broker}"),
                "",
                "[[account]]",
            ),
            (
                format!("[[turn]]\nurls = [\"turn:r:3478\"]\nsecret = \"s\"\n{broker}"),
                "",
                "[[turn]]",
            ),
        ] {
            let error = format!(
                "{:#}",
                parsed(&text, more)
                    .err()
                    .unwrap_or_else(|| panic!("{text} {more}"))
            );
            assert!(
                error.contains("through the broker only") && error.contains(named),
                "{error}"
            );
        }
        // Without a broker, the host serves its own pages, as before.
        let direct = parsed("[listen]\naddress = \"0.0.0.0:8090\"\n", "").unwrap();
        assert!(direct.serves_pages());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
