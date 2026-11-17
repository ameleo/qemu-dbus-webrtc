// SPDX-License-Identifier: MPL-2.0

//! The host's TLS client identity to its broker: the certificate the broker's ACME server issued
//! it (`host.pem`), and its key: in the TPM, which signs through a thread of its own (the TPM's
//! context does not move between threads), or in a file for a host without one.
//!
//! Both are read again for each connection, so that a renewed certificate is taken at once.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};
use tracing::warn;

use super::Settings;
use super::tpm::{Blobs, Tpm};

pub struct Identity {
    state: PathBuf,
    tpm: Option<TpmWorker>,
}

impl Identity {
    /// The identity of `settings`: with a TPM, its thread starts now.
    pub fn new(settings: &Settings) -> Result<Identity> {
        let tpm = match &settings.tpm {
            Some(tcti) => Some(TpmWorker::start(tcti)?),
            None => None,
        };
        Ok(Identity {
            state: settings.state.clone(),
            tpm,
        })
    }

    /// The certificate, its chain, and the key, as they are now.
    pub fn certified_key(&self) -> Result<Arc<CertifiedKey>> {
        let path = self.state.join("host.pem");
        let pem = std::fs::read(&path).with_context(|| {
            format!(
                "{}: no certificate yet (qemu-dbus-webrtc enroll)",
                path.display()
            )
        })?;
        let chain = certificates(&pem)?;
        let key: Arc<dyn SigningKey> = match &self.tpm {
            Some(tpm) => {
                let blobs = std::fs::read(self.state.join("host-key.blobs"))
                    .context("the host's key in the TPM")?;
                tpm.load(blobs)?;
                Arc::new(TpmKey(tpm.clone()))
            }
            None => {
                let pkcs8 =
                    std::fs::read(self.state.join("host-key.pk8")).context("the host's key")?;
                rustls::crypto::ring::sign::any_ecdsa_type(&PrivateKeyDer::Pkcs8(
                    PrivatePkcs8KeyDer::from(pkcs8),
                ))
                .map_err(|e| anyhow!("the host's key: {e}"))?
            }
        };
        Ok(Arc::new(CertifiedKey::new(chain, key)))
    }

    /// When to renew the certificate: two thirds of the way through its validity; now if there
    /// is none, or it cannot be read.
    pub fn renew_at(&self) -> SystemTime {
        let read = || -> Result<SystemTime> {
            let pem = std::fs::read(self.state.join("host.pem"))?;
            let chain = certificates(&pem)?;
            let leaf = chain.first().context("an empty certificate file")?;
            let (_, certificate) = x509_parser::parse_x509_certificate(leaf)?;
            let validity = certificate.validity();
            let from = validity.not_before.timestamp();
            let to = validity.not_after.timestamp();
            let at = from + (to - from) * 2 / 3;
            Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(at)?))
        };
        read().unwrap_or_else(|e| {
            warn!(error = %format!("{e:#}"), "the host's certificate: to get again");
            SystemTime::now()
        })
    }
}

/// The certificates of a PEM file, in order.
fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    let chain = CertificateDer::pem_slice_iter(pem).collect::<Result<Vec<_>, _>>()?;
    if chain.is_empty() {
        bail!("no certificate");
    }
    Ok(chain)
}

/// What the TPM's thread is asked.
enum Job {
    /// Loads the host's key from its blobs, unless it is the one loaded.
    Load(Vec<u8>, mpsc::Sender<Result<()>>),
    /// An ECDSA signature of a SHA-256 digest by the loaded key: r and s.
    Sign([u8; 32], mpsc::Sender<Result<([u8; 32], [u8; 32])>>),
}

/// The thread that holds the TPM's context and the host's key loaded in it.
#[derive(Clone)]
struct TpmWorker {
    jobs: mpsc::Sender<Job>,
}

impl std::fmt::Debug for TpmWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TpmWorker")
    }
}

impl TpmWorker {
    fn start(tcti: &str) -> Result<TpmWorker> {
        let (jobs, received) = mpsc::channel::<Job>();
        let (ready, opened) = mpsc::channel();
        let tcti = tcti.to_owned();
        std::thread::Builder::new()
            .name("tpm".into())
            .spawn(move || {
                let mut tpm = match Tpm::open(&tcti) {
                    Ok(tpm) => {
                        let _ = ready.send(Ok(()));
                        tpm
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let mut loaded: Option<(Vec<u8>, tss_esapi::handles::KeyHandle)> = None;
                for job in received {
                    match job {
                        Job::Load(blobs, answer) => {
                            if loaded.as_ref().is_some_and(|(kept, _)| *kept == blobs) {
                                let _ = answer.send(Ok(()));
                                continue;
                            }
                            if let Some((_, handle)) = loaded.take() {
                                tpm.flush(handle);
                            }
                            let mut load = || -> Result<tss_esapi::handles::KeyHandle> {
                                let key = Blobs::from_bytes(&blobs)?;
                                let srk = tpm.srk()?;
                                let handle = tpm.load(srk, &key);
                                tpm.flush(srk);
                                handle
                            };
                            let result = load().map(|handle| loaded = Some((blobs, handle)));
                            let _ = answer.send(result);
                        }
                        Job::Sign(digest, answer) => {
                            let result = match &loaded {
                                Some((_, handle)) => tpm.sign(*handle, &digest),
                                None => Err(anyhow!("no key loaded in the TPM")),
                            };
                            let _ = answer.send(result);
                        }
                    }
                }
                if let Some((_, handle)) = loaded {
                    tpm.flush(handle);
                }
            })?;
        opened.recv().context("the TPM's thread")??;
        Ok(TpmWorker { jobs })
    }

    fn load(&self, blobs: Vec<u8>) -> Result<()> {
        let (answer, result) = mpsc::channel();
        self.jobs
            .send(Job::Load(blobs, answer))
            .map_err(|_| anyhow!("the TPM's thread ended"))?;
        result
            .recv()
            .map_err(|_| anyhow!("the TPM's thread ended"))?
    }

    fn sign(&self, digest: [u8; 32]) -> Result<([u8; 32], [u8; 32])> {
        let (answer, result) = mpsc::channel();
        self.jobs
            .send(Job::Sign(digest, answer))
            .map_err(|_| anyhow!("the TPM's thread ended"))?;
        result
            .recv()
            .map_err(|_| anyhow!("the TPM's thread ended"))?
    }
}

/// The host's key in the TPM, as rustls signs with it: ECDSA P-256 with SHA-256.
#[derive(Debug, Clone)]
struct TpmKey(TpmWorker);

impl SigningKey for TpmKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(self.clone()) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

impl Signer for TpmKey {
    /// An ECDSA-Sig-Value in DER, as TLS carries it (RFC 8446 4.2.3).
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let digest: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, message)
            .as_ref()
            .try_into()
            .expect("32 bytes");
        let (r, s) = self
            .0
            .sign(digest)
            .map_err(|e| rustls::Error::General(format!("the TPM's signature: {e:#}")))?;
        Ok(super::csr::ecdsa_signature(&r, &s))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// Always this certificate and key, for the broker.
#[derive(Debug)]
pub struct Resolver(pub Arc<CertifiedKey>);

impl rustls::client::ResolvesClientCert for Resolver {
    fn resolve(&self, _hints: &[&[u8]], schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        self.0.key.choose_scheme(schemes).map(|_| self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}
