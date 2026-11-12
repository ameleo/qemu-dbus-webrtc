// SPDX-License-Identifier: MPL-2.0

//! The VMs of libvirt, through libvirt-dbus (`org.libvirt` on the system bus): every domain
//! with a D-Bus display (`<graphics type='dbus'>`), running or not, by UUID and name; the bus
//! of a running one's display, from its live definition; and starting one that is off, when
//! the configuration allows it. Without libvirt-dbus, the displays' sockets alone: the running
//! domains only, and their UUID read from the VMs.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use qdw_qemu::display::Attach;
use tokio::sync::OnceCell;
use tracing::{info, warn};
use zbus::zvariant::OwnedObjectPath;

use crate::access::{Account, Accounts};
use crate::providers::{Candidate, Provider};

#[zbus::proxy(
    interface = "org.libvirt.Connect",
    default_service = "org.libvirt",
    default_path = "/org/libvirt/QEMU"
)]
trait Connect {
    fn list_domains(&self, flags: u32) -> zbus::Result<Vec<OwnedObjectPath>>;
    #[zbus(name = "DomainLookupByUUID")]
    fn domain_lookup_by_uuid(&self, uuid: &str) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.libvirt.Domain", default_service = "org.libvirt")]
trait Domain {
    fn create(&self, flags: u32) -> zbus::Result<()>;
    fn set_metadata(
        &self,
        kind: i32,
        metadata: &str,
        key: &str,
        uri: &str,
        flags: u32,
    ) -> zbus::Result<()>;
    #[zbus(name = "GetXMLDesc")]
    fn get_xml_desc(&self, flags: u32) -> zbus::Result<String>;
    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> zbus::Result<String>;
}

pub struct Libvirt {
    /// Where libvirt puts the displays' sockets: what is read without libvirt-dbus.
    pub dir: PathBuf,
    /// Start a domain that is off when a client asks for it.
    pub start: bool,
    /// The system bus, if libvirt-dbus answers on it; looked for once.
    libvirt: OnceCell<Option<zbus::Connection>>,
}

impl Libvirt {
    pub fn new(dir: PathBuf, start: bool) -> Self {
        Self {
            dir,
            start,
            libvirt: OnceCell::new(),
        }
    }

    /// The connection to libvirt-dbus, or `None` without it.
    async fn libvirt(&self) -> Option<&zbus::Connection> {
        self.libvirt
            .get_or_init(|| async {
                let found = async {
                    let system = zbus::Connection::system().await?;
                    ConnectProxy::new(&system).await?.list_domains(0).await?;
                    anyhow::Ok(system)
                };
                match found.await {
                    Ok(system) => {
                        info!("libvirt domains through libvirt-dbus");
                        Some(system)
                    }
                    Err(e) => {
                        warn!(
                            error = %format!("{e:#}"),
                            "no libvirt-dbus: only the running domains are seen, and none is started"
                        );
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    async fn domains(&self, system: &zbus::Connection) -> Result<Vec<Candidate>> {
        let mut candidates = Vec::new();
        let paths = ConnectProxy::new(system)
            .await?
            .list_domains(0)
            .await
            .context("listing the domains")?;
        for path in paths {
            let domain = domain(system, path).await?;
            // The live definition of a running domain: its display's bus is in it.
            let xml = domain
                .get_xml_desc(0)
                .await
                .context("reading a domain's definition")?;
            let Some(graphics) = dbus_graphics(&xml) else {
                continue;
            };
            let attach = if domain
                .active()
                .await
                .context("reading whether a domain runs")?
            {
                // A peer-to-peer display has no bus to join.
                let Some(address) = attribute(graphics, "address") else {
                    continue;
                };
                Some(Attach::Bus {
                    address: address.to_owned(),
                })
            } else {
                None
            };
            candidates.push(Candidate {
                attach,
                uuid: Some(domain.uuid().await.context("reading a domain's UUID")?),
                names: vec![domain.name().await.context("reading a domain's name")?],
                startable: self.start,
            });
        }
        Ok(candidates)
    }

    /// The running domains, from their displays' sockets in `dir`, `<id>-<domain>-dbus.sock`.
    fn sockets(&self) -> Result<Vec<Candidate>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // No domain with a D-Bus display runs.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        Ok(entries
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_string_lossy().into_owned();
                Some(Candidate {
                    names: domain_of_socket(&file)?.into_iter().collect(),
                    attach: Some(Attach::Bus {
                        address: format!("unix:path={}", e.path().display()),
                    }),
                    uuid: None,
                    startable: false,
                })
            })
            .collect())
    }
}

impl Provider for Libvirt {
    fn name(&self) -> &'static str {
        "libvirt"
    }

    fn candidates(&self) -> BoxFuture<'_, Result<Vec<Candidate>>> {
        async move {
            match self.libvirt().await {
                Some(system) => self.domains(system).await,
                None => self.sockets(),
            }
        }
        .boxed()
    }

    fn start<'a>(&'a self, uuid: &'a str) -> BoxFuture<'a, Result<()>> {
        async move {
            let system = self
                .libvirt()
                .await
                .context("starting a domain takes libvirt-dbus")?;
            let path = ConnectProxy::new(system)
                .await?
                .domain_lookup_by_uuid(uuid)
                .await?;
            let domain = domain(system, path).await?;
            let name = domain.name().await?;
            info!(uuid, %name, "starting the domain");
            domain.create(0).await.context("starting the domain")?;
            Ok(())
        }
        .boxed()
    }

    fn accounts<'a>(&'a self, uuid: &'a str) -> BoxFuture<'a, Result<Option<Accounts>>> {
        async move {
            let Some(system) = self.libvirt().await else {
                return Ok(None);
            };
            let domain = self.lookup(system, uuid).await?;
            // From the definition rather than GetMetadata, whose error for no metadata cannot
            // be told from another (it is translated): a failure stays one, and is not taken
            // for no account.
            let xml = domain
                .get_xml_desc(0)
                .await
                .context("reading the domain's definition")?;
            Ok(Some(accounts_of(&xml, uuid)))
        }
        .boxed()
    }

    fn set_accounts<'a>(
        &'a self,
        uuid: &'a str,
        accounts: &'a Accounts,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            let system = self
                .libvirt()
                .await
                .context("keeping accounts with a domain takes libvirt-dbus")?;
            let domain = self.lookup(system, uuid).await?;
            // In its definition, and in the running domain too when it runs.
            let flags = if domain.active().await? {
                AFFECT_LIVE | AFFECT_CONFIG
            } else {
                AFFECT_CONFIG
            };
            let xml = accounts_xml(accounts, uuid);
            domain
                .set_metadata(METADATA_ELEMENT, &xml, METADATA_PREFIX, METADATA_URI, flags)
                .await
                .context("writing the domain's accounts")?;
            Ok(())
        }
        .boxed()
    }
}

impl Libvirt {
    async fn lookup(&self, system: &zbus::Connection, uuid: &str) -> Result<DomainProxy<'static>> {
        let path = ConnectProxy::new(system)
            .await?
            .domain_lookup_by_uuid(uuid)
            .await?;
        domain(system, path).await
    }
}

/// The namespace of our metadata in a domain's definition, and its prefix there.
const METADATA_URI: &str = "https://github.com/ameleo/qemu-dbus-webrtc";
const METADATA_PREFIX: &str = "qdw";
/// virDomainMetadataType: an element of ours.
const METADATA_ELEMENT: i32 = 2;
/// virDomainModificationImpact.
const AFFECT_LIVE: u32 = 1;
const AFFECT_CONFIG: u32 = 2;

/// The accounts in a domain's definition: our metadata element,
/// `<qdw:accounts xmlns:qdw="..."><qdw:account user="..." password="..." shadow-only="yes"/>`.
/// None when it has none.
fn accounts_of(xml: &str, uuid: &str) -> Accounts {
    let mut accounts = Accounts::default();
    let Some(start) = xml
        .find(&format!("=\"{METADATA_URI}\""))
        .or_else(|| xml.find(&format!("='{METADATA_URI}'")))
    else {
        return accounts;
    };
    // The element our namespace is declared on, and what follows it until its end.
    let Some(open) = xml[..start].rfind('<') else {
        return accounts;
    };
    let element = &xml[open..];
    let Some(prefix) = element[1..]
        .split(|c: char| c.is_whitespace() || c == '>')
        .next()
        .and_then(|name| name.split_once(':'))
        .map(|(prefix, _)| prefix)
    else {
        return accounts;
    };
    let end = element
        .find(&format!("</{prefix}:accounts>"))
        .unwrap_or(element.len());
    let tag = format!("<{prefix}:account ");
    let mut rest = &element[..end];
    while let Some(at) = rest.find(&tag) {
        let account = &rest[at..];
        let close = account.find('>').unwrap_or(account.len());
        let attrs = &account[..close];
        if let (Some(user), Some(password)) =
            (attribute(attrs, "user"), attribute(attrs, "password"))
        {
            accounts.accounts.push(Account {
                vm: uuid.to_ascii_lowercase(),
                user: unescape(user),
                password: unescape(password),
                shadow_only: attribute(attrs, "shadow-only") == Some("yes"),
            });
        }
        rest = &account[close..];
    }
    accounts
}

/// Our metadata element with `accounts` of the VM `uuid`, as SetMetadata takes it.
fn accounts_xml(accounts: &Accounts, uuid: &str) -> String {
    let mut xml = String::from("<accounts>");
    for account in accounts.of(uuid) {
        xml += &format!(
            "<account user=\"{}\" password=\"{}\"{}/>",
            escape(&account.user),
            escape(&account.password),
            if account.shadow_only {
                " shadow-only=\"yes\""
            } else {
                ""
            }
        );
    }
    xml + "</accounts>"
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The domain at `path`. Its properties are read when asked, not cached: zbus's cache fails
/// on some of libvirt's domain properties ("incorrect type").
async fn domain(system: &zbus::Connection, path: OwnedObjectPath) -> Result<DomainProxy<'static>> {
    Ok(DomainProxy::builder(system)
        .path(path)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}

/// The D-Bus `<graphics>` element of a domain definition, up to its first `>`.
fn dbus_graphics(xml: &str) -> Option<&str> {
    let mut rest = xml;
    while let Some(start) = rest.find("<graphics") {
        let element = &rest[start..];
        let end = element.find('>')?;
        let tag = &element[..end];
        if attribute(tag, "type") == Some("dbus") {
            return Some(tag);
        }
        rest = &element[end..];
    }
    None
}

/// The value of an XML attribute in a start tag, quoted either way.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['\'', '"'] {
        let key = format!(" {name}={quote}");
        if let Some(start) = tag.find(&key) {
            let value = &tag[start + key.len()..];
            return value.find(quote).map(|end| &value[..end]);
        }
    }
    None
}

/// The domain of a libvirt display socket, `<id>-<domain>-dbus.sock`: `None` for another file,
/// `Some(None)` for a socket not named so.
fn domain_of_socket(file: &str) -> Option<Option<String>> {
    let rest = file.strip_suffix("-dbus.sock")?;
    let Some((id, domain)) = rest.split_once('-') else {
        return Some(None);
    };
    let numbered = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    Some((numbered && !domain.is_empty()).then(|| domain.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_of_a_definition() {
        let live = "<domain type='kvm'>\n  <graphics type='spice' port='5900'/>\n  \
                    <graphics type='dbus' address='unix:path=/run/libvirt/qemu/dbus/6-win-dbus.sock'>\n  \
                    <gl enable='yes'/>\n  </graphics>\n</domain>";
        let graphics = dbus_graphics(live).unwrap();
        assert_eq!(
            attribute(graphics, "address"),
            Some("unix:path=/run/libvirt/qemu/dbus/6-win-dbus.sock")
        );
        // Off: no address yet.
        let inactive = "<domain>\n  <graphics type=\"dbus\">\n  </graphics>\n</domain>";
        assert_eq!(attribute(dbus_graphics(inactive).unwrap(), "address"), None);
        assert_eq!(
            dbus_graphics("<domain><graphics type='vnc'/></domain>"),
            None
        );
    }

    #[test]
    fn accounts_in_a_definition() {
        const VM: &str = "b8ff54ee-77f5-43dd-aa19-de8eaed5eb75";
        let mut accounts = Accounts::default();
        accounts.accounts.push(Account {
            vm: VM.into(),
            user: "o'brien & co".into(),
            password: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            shadow_only: false,
        });
        accounts.accounts.push(Account {
            vm: VM.into(),
            user: "support".into(),
            password: "$argon2id$x".into(),
            shadow_only: true,
        });
        // As libvirt gives it back: the element in our namespace, with its prefix.
        let inner = accounts_xml(&accounts, VM)
            .replace("<account ", "<qdw:account ")
            .replace(
                "<accounts>",
                &format!("<qdw:accounts xmlns:qdw=\"{METADATA_URI}\">"),
            )
            .replace("</accounts>", "</qdw:accounts>");
        let xml = format!("<domain>\n  <metadata>\n    {inner}\n  </metadata>\n</domain>");
        let read = accounts_of(&xml, VM);
        assert_eq!(read.accounts.len(), 2);
        assert_eq!(read.accounts[0].user, "o'brien & co");
        assert_eq!(read.accounts[0].password, accounts.accounts[0].password);
        assert!(!read.accounts[0].shadow_only);
        assert!(read.accounts[1].shadow_only);
        assert!(
            accounts_of("<domain><metadata/></domain>", VM)
                .accounts
                .is_empty()
        );
    }

    #[test]
    fn domains_by_their_socket() {
        assert_eq!(
            domain_of_socket("6-daas-win-dbus-dbus.sock"),
            Some(Some("daas-win-dbus".into()))
        );
        assert_eq!(domain_of_socket("6-daas-win-dbus-dbus.pid"), None);
        assert_eq!(domain_of_socket("x-dbus.sock"), Some(None));
    }
}
