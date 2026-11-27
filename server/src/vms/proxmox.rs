// SPDX-License-Identifier: MPL-2.0

//! The VMs of Proxmox VE, from their QMP sockets and configurations. Not tried yet.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use qdw_qemu::display::Attach;

use crate::vms::providers::{Candidate, Provider};

/// Proxmox VE: each VM's QMP socket is `<vmid>.qmp` in `run_dir`, and its UUID is in its
/// configuration, `<vmid>.conf` in `conf_dir` (`smbios1: uuid=...`). The VM must run with
/// `-display dbus,p2p=on` (`args:` in its configuration). Not tried yet.
pub struct Proxmox {
    pub run_dir: PathBuf,
    pub conf_dir: PathBuf,
}

impl Provider for Proxmox {
    fn name(&self) -> &'static str {
        "proxmox"
    }

    fn candidates(&self) -> BoxFuture<'_, Result<Vec<Candidate>>> {
        async move { self.running() }.boxed()
    }
}

impl Proxmox {
    fn running(&self) -> Result<Vec<Candidate>> {
        let entries = match std::fs::read_dir(&self.run_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.run_dir.display())),
        };
        Ok(entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let vmid = name.strip_suffix(".qmp")?;
                let conf = std::fs::read_to_string(self.conf_dir.join(format!("{vmid}.conf")))
                    .unwrap_or_default();
                // Its VM id, and its name.
                let mut names = vec![vmid.to_owned()];
                names.extend(proxmox_field(&conf, "name").map(str::to_owned));
                Some(Candidate {
                    attach: Some(Attach::P2p { qmp: e.path() }),
                    uuid: proxmox_field(&conf, "smbios1").and_then(|smbios| {
                        smbios
                            .split(',')
                            .find_map(|field| field.trim().strip_prefix("uuid="))
                            .map(str::to_owned)
                    }),
                    names,
                    startable: false,
                })
            })
            .collect())
    }
}

/// A field of a Proxmox VE VM configuration, `<field>: <value>`, in its main section (before
/// the snapshots').
fn proxmox_field<'a>(conf: &'a str, field: &str) -> Option<&'a str> {
    conf.lines()
        .take_while(|line| !line.starts_with('['))
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxmox_vm() {
        let dir = std::env::temp_dir().join(format!("qdw-proxmox-{}", std::process::id()));
        let (run_dir, conf_dir) = (dir.join("run"), dir.join("conf"));
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(run_dir.join("101.qmp"), "").unwrap();
        std::fs::write(run_dir.join("101.pid"), "").unwrap();
        std::fs::write(
            conf_dir.join("101.conf"),
            "boot: order=scsi0\nname: win11\n\
             smbios1: uuid=7b3a1c2d-0000-4000-8000-123456789abc,manufacturer=QQ\n\
             [snap]\nname: old\nsmbios1: uuid=00000000-0000-0000-0000-000000000000\n",
        )
        .unwrap();
        let candidates = Proxmox { run_dir, conf_dir }.running().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].uuid.as_deref(),
            Some("7b3a1c2d-0000-4000-8000-123456789abc")
        );
        assert_eq!(candidates[0].names, ["101", "win11"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
