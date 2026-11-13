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
use tracing::{debug, info, warn};
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
    /// The system bus, once libvirt-dbus answered on it; looked for again while it has not,
    /// and after a failure (libvirtd started after the server, the bus restarted).
    libvirt: tokio::sync::Mutex<Option<zbus::Connection>>,
    /// libvirt-dbus was found missing: said once, not at every look.
    missing_said: std::sync::atomic::AtomicBool,
}

impl Libvirt {
    pub fn new(dir: PathBuf, start: bool) -> Self {
        Self {
            dir,
            start,
            libvirt: Default::default(),
            missing_said: Default::default(),
        }
    }

    /// The connection to libvirt-dbus, or `None` without it.
    async fn libvirt(&self) -> Option<zbus::Connection> {
        let mut libvirt = self.libvirt.lock().await;
        if let Some(system) = libvirt.as_ref() {
            return Some(system.clone());
        }
        let found = async {
            let system = zbus::Connection::system().await?;
            ConnectProxy::new(&system).await?.list_domains(0).await?;
            anyhow::Ok(system)
        };
        match found.await {
            Ok(system) => {
                info!("libvirt domains through libvirt-dbus");
                self.missing_said
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                *libvirt = Some(system.clone());
                Some(system)
            }
            Err(e) => {
                if !self
                    .missing_said
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    warn!(
                        error = %format!("{e:#}"),
                        "no libvirt-dbus: only the running domains are seen, and none is started"
                    );
                }
                None
            }
        }
    }

    /// libvirt-dbus failed: looked for again at the next call.
    async fn forget_libvirt(&self) {
        *self.libvirt.lock().await = None;
    }

    /// The candidate of the domain at `path`: `None` for one without a D-Bus display, or a
    /// running one whose display is peer to peer.
    async fn domain_candidate(
        &self,
        system: &zbus::Connection,
        path: OwnedObjectPath,
    ) -> Result<Option<Candidate>> {
        let domain = domain(system, path).await?;
        // The live definition of a running domain: its display's bus is in it.
        let xml = domain
            .get_xml_desc(0)
            .await
            .context("reading a domain's definition")?;
        let Some(graphics) = dbus_graphics(&xml) else {
            return Ok(None);
        };
        let attach = if domain
            .active()
            .await
            .context("reading whether a domain runs")?
        {
            // A peer-to-peer display has no bus to join.
            let Some(address) = attribute(graphics, "address") else {
                return Ok(None);
            };
            Some(Attach::Bus { address })
        } else {
            None
        };
        Ok(Some(Candidate {
            attach,
            uuid: Some(domain.uuid().await.context("reading a domain's UUID")?),
            names: vec![domain.name().await.context("reading a domain's name")?],
            startable: self.start,
        }))
    }

    async fn domains(&self, system: &zbus::Connection) -> Result<Vec<Candidate>> {
        let paths = ConnectProxy::new(system)
            .await?
            .list_domains(0)
            .await
            .context("listing the domains")?;
        let mut candidates = Vec::new();
        for path in paths {
            // A domain that fails to answer (undefined meanwhile) is left out, not the others.
            match self.domain_candidate(system, path).await {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(e) => debug!(error = %format!("{e:#}"), "domain left out"),
            }
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
                Some(system) => match self.domains(&system).await {
                    Ok(candidates) => Ok(candidates),
                    Err(e) => {
                        self.forget_libvirt().await;
                        Err(e)
                    }
                },
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
            let path = ConnectProxy::new(&system)
                .await?
                .domain_lookup_by_uuid(uuid)
                .await?;
            let domain = domain(&system, path).await?;
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
        if attribute(tag, "type").as_deref() == Some("dbus") {
            return Some(tag);
        }
        rest = &element[end..];
    }
    None
}

/// The value of an XML attribute in a start tag, quoted either way, its entities decoded.
fn attribute(tag: &str, name: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        let key = format!(" {name}={quote}");
        if let Some(start) = tag.find(&key) {
            let value = &tag[start + key.len()..];
            return value.find(quote).map(|end| unescape(&value[..end]));
        }
    }
    None
}

/// XML's predefined entities decoded (`&amp;` last, so that `&amp;lt;` stays `&lt;`).
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
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
            attribute(graphics, "address").as_deref(),
            Some("unix:path=/run/libvirt/qemu/dbus/6-win-dbus.sock")
        );
        let escaped =
            "<graphics type='dbus' address='unix:path=/run/a&amp;b&apos;c&amp;lt;d.sock'>";
        assert_eq!(
            attribute(dbus_graphics(escaped).unwrap(), "address").as_deref(),
            Some("unix:path=/run/a&b'c&lt;d.sock")
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
