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
    fn domains_by_their_socket() {
        assert_eq!(
            domain_of_socket("6-daas-win-dbus-dbus.sock"),
            Some(Some("daas-win-dbus".into()))
        );
        assert_eq!(domain_of_socket("6-daas-win-dbus-dbus.pid"), None);
        assert_eq!(domain_of_socket("x-dbus.sock"), Some(None));
    }
}
