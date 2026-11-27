// SPDX-License-Identifier: MPL-2.0

//! The server's certificate from an ACME certificate authority (RFC 8555): a public one, or the
//! organization's own (step-ca, OpenBao...), with an account bound to one the authority already
//! knows (External Account Binding) when it asks for it.
//!
//! An authorization the authority already holds as valid (a domain validated for the account
//! beforehand, as some do with EAB) takes no challenge. Otherwise HTTP-01: the authority fetches
//! `http://<domain>/.well-known/acme-challenge/<token>`, which the server answers on its own
//! port 80 for the time of the validation; a private authority reaches servers on private
//! addresses. The certificate is renewed once two thirds of its lifetime have gone, and the
//! pages' TLS takes it without a restart.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::routing::get;
use base64::Engine;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, ExternalAccountKey,
    Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The HTTP-01 challenges being answered: their tokens, and the answers.
type Tokens = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// How long the authority has to validate the challenges, and to issue the certificate.
const VALIDATION_TIME: Duration = Duration::from_secs(120);
/// How long a whole renewal may take: an authority that takes the connection and never answers
/// would hold it for ever (the ACME client sets no time on its requests), and the renewals
/// after it with it.
const RENEWAL_TIME: Duration = Duration::from_secs(300);
/// After a failed renewal, how long before the next try.
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The longest wait between two looks at the certificate.
const LONGEST_WAIT: Duration = Duration::from_secs(12 * 3600);

/// The ACME settings.
#[derive(Debug, Clone)]
pub struct Acme {
    /// The authority's directory URL.
    pub directory: String,
    /// The names, and the IP addresses (RFC 8738, when the authority takes them), the
    /// certificate is for.
    pub domains: Vec<String>,
    /// Contact URIs of the account (`mailto:...`).
    pub contact: Vec<String>,
    /// External Account Binding: the key id, and the MAC key (base64url, as authorities give
    /// it).
    pub eab: Option<(String, String)>,
    /// The roots that sign the authority's own HTTPS certificate, for a private one.
    pub ca_bundle: Option<PathBuf>,
    /// Where the HTTP-01 challenges are answered.
    pub http: SocketAddr,
    /// Where the account is kept.
    pub dir: PathBuf,
    /// Where the certificate and its key go.
    pub certificate: PathBuf,
    pub key: PathBuf,
}

/// The account, as kept on disk: with the directory it is of, so that another directory gets
/// another account.
#[derive(Serialize, Deserialize)]
struct Kept {
    directory: String,
    credentials: AccountCredentials,
}

impl Acme {
    /// When the certificate in place is due for renewal: now if there is none, or it cannot be
    /// read, or it is not for the names asked.
    pub fn due(&self) -> SystemTime {
        match self.lifetime() {
            Ok((not_before, not_after)) => {
                let lifetime = not_after.duration_since(not_before).unwrap_or_default();
                not_before + lifetime * 2 / 3
            }
            Err(e) => {
                info!(error = %format!("{e:#}"), "no certificate to keep: one is asked for");
                SystemTime::UNIX_EPOCH
            }
        }
    }

    /// Whether the certificate in place can serve: for every name asked, its key beside it, and
    /// not expired yet. A server starts with it, and leaves its renewal to [`keep`].
    pub fn usable(&self) -> bool {
        self.lifetime()
            .is_ok_and(|(_, not_after)| SystemTime::now() < not_after)
    }

    /// The validity of the certificate in place, if it is for every name asked.
    fn lifetime(&self) -> Result<(SystemTime, SystemTime)> {
        let pem_bytes = std::fs::read(&self.certificate)
            .with_context(|| format!("reading {}", self.certificate.display()))?;
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(&pem_bytes).context("not a PEM certificate")?;
        let certificate = pem.parse_x509().context("not a certificate")?;
        let names: Vec<String> = certificate
            .subject_alternative_name()
            .ok()
            .flatten()
            .map(|san| {
                san.value
                    .general_names
                    .iter()
                    .filter_map(|name| match name {
                        x509_parser::extensions::GeneralName::DNSName(dns) => Some(name_of(dns)),
                        x509_parser::extensions::GeneralName::IPAddress(ip) => {
                            ip_of(ip).map(|ip| ip.to_string())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(missing) = self.domains.iter().find(|d| !names.contains(&name_of(d))) {
            bail!("the certificate in place is not for {missing}");
        }
        // The key beside it must be its own: written one after the other, a crash between the
        // two leaves a new key beside the old certificate.
        let chain = CertificateDer::pem_slice_iter(&pem_bytes)
            .collect::<Result<Vec<_>, _>>()
            .context("reading the certificate chain")?;
        let key = PrivateKeyDer::from_pem_file(&self.key)
            .with_context(|| format!("reading {}", self.key.display()))?;
        rustls::sign::CertifiedKey::from_der(chain, key, &rustls::crypto::ring::default_provider())
            .context("the key in place is not the certificate's")?;
        let validity = certificate.validity();
        let time = |t: x509_parser::time::ASN1Time| {
            SystemTime::UNIX_EPOCH + Duration::from_secs(t.timestamp().max(0) as u64)
        };
        Ok((time(validity.not_before), time(validity.not_after)))
    }

    /// Get a certificate from the authority, and write it with its key: within `RENEWAL_TIME`.
    pub async fn renew(&self) -> Result<()> {
        tokio::time::timeout(RENEWAL_TIME, self.order())
            .await
            .context("the authority did not answer in time")?
    }

    async fn order(&self) -> Result<()> {
        let account = self.account().await?;
        let identifiers: Vec<Identifier> = self
            .domains
            .iter()
            .map(|d| match d.parse::<IpAddr>() {
                Ok(ip) => Identifier::Ip(ip),
                Err(_) => Identifier::Dns(d.clone()),
            })
            .collect();
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .context("ordering the certificate")?;

        // The challenges to answer: none for what the authority already holds as valid.
        let tokens: Tokens = Arc::default();
        let mut challenges = Vec::new();
        {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                match authorization.status {
                    AuthorizationStatus::Valid => continue,
                    AuthorizationStatus::Pending => {}
                    status => bail!("authorization {status:?}"),
                }
                let name = authorization.identifier().to_string();
                let challenge = authorization
                    .challenge(ChallengeType::Http01)
                    .with_context(|| format!("no HTTP-01 challenge offered for {name}"))?;
                tokens.lock().unwrap().push((
                    challenge.token.clone(),
                    challenge.key_authorization().as_str().to_owned(),
                ));
                challenges.push(name);
            }
        }

        if challenges.is_empty() {
            info!("every name already validated for the account: no challenge");
        } else {
            // Answer the challenges, then tell the authority they are ready.
            // Stopped however this ends: left running, it would hold the port for every later
            // try. UNTESTED: an error while the challenges are served (needs a failing
            // authority).
            let _server = AbortOnDrop(serve_challenges(self.http, tokens.clone()).await?);
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization?;
                if authorization.status != AuthorizationStatus::Pending {
                    continue;
                }
                if let Some(mut challenge) = authorization.challenge(ChallengeType::Http01) {
                    challenge
                        .set_ready()
                        .await
                        .context("telling the challenge ready")?;
                }
            }
            let status = order
                .poll_ready(&RetryPolicy::new().timeout(VALIDATION_TIME))
                .await;
            match status.context("waiting for the validation")? {
                OrderStatus::Ready => {}
                status => bail!("the order is {status:?}: the validation failed"),
            }
        }

        let key = order.finalize().await.context("finalizing the order")?;
        let chain = order
            .poll_certificate(&RetryPolicy::new().timeout(VALIDATION_TIME))
            .await
            .context("getting the certificate")?;
        // Both written and synced beside their places, then put in place one right after the
        // other: a crash while writing leaves the pair in place as it was. Between the two
        // renames, the new key is beside the old certificate: `lifetime` tells, and the pair is
        // renewed.
        let new_key = write_beside(&self.key, key.as_bytes())?;
        let new_certificate = write_beside(&self.certificate, chain.as_bytes())?;
        put_in_place(&new_key, &self.key)?;
        put_in_place(&new_certificate, &self.certificate)?;
        info!(domains = ?self.domains, certificate = %self.certificate.display(), "certificate renewed");
        Ok(())
    }

    /// The account: the one kept for this directory, or a new one (bound to the external
    /// account, if any), then kept.
    async fn account(&self) -> Result<Account> {
        let path = self.dir.join("account.json");
        let builder = || match &self.ca_bundle {
            Some(roots) => Account::builder_with_root(roots)
                .with_context(|| format!("reading {}", roots.display())),
            None => Account::builder().context("the ACME client"),
        };
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(kept) = serde_json::from_str::<Kept>(&text)
            && kept.directory == self.directory
        {
            return builder()?
                .from_credentials(kept.credentials)
                .await
                .context("restoring the ACME account");
        }
        let eab = match &self.eab {
            Some((kid, hmac)) => {
                let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(hmac.trim_end_matches('='))
                    .context("the EAB MAC key is not base64url")?;
                Some(ExternalAccountKey::new(kid.clone(), &key))
            }
            None => None,
        };
        let contact: Vec<&str> = self.contact.iter().map(String::as_str).collect();
        let (account, credentials) = builder()?
            .create(
                &NewAccount {
                    contact: &contact,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory.clone(),
                eab.as_ref(),
            )
            .await
            .context("creating the ACME account")?;
        let kept = Kept {
            directory: self.directory.clone(),
            credentials,
        };
        write_private(&path, serde_json::to_string(&kept)?.as_bytes())?;
        info!(directory = self.directory, "ACME account created");
        Ok(account)
    }
}

/// The certificate in place, renewed when due, until the server ends; `reload` takes it into
/// the pages' TLS each time.
pub async fn keep(acme: Acme, reload: impl Fn() + Send + 'static) {
    loop {
        let due = acme.due();
        let wait = due
            .duration_since(SystemTime::now())
            .unwrap_or_default()
            .min(LONGEST_WAIT);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
            continue;
        }
        match acme.renew().await {
            Ok(()) => {
                reload();
                // A certificate due at once (not for every name asked, or already old): asked
                // again at once, it would be again and again, up to the authority's limits.
                // UNTESTED: needs an authority issuing such a certificate.
                if acme.due() <= SystemTime::now() {
                    warn!(
                        "the new certificate is due for renewal already: trying again in an hour"
                    );
                    tokio::time::sleep(RETRY_AFTER).await;
                }
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "certificate not renewed: trying again in an hour");
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
    }
}

/// A task aborted when this is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Answer the HTTP-01 challenges in `tokens` on `address`, until the returned task is aborted.
async fn serve_challenges(
    address: SocketAddr,
    tokens: Tokens,
) -> Result<tokio::task::JoinHandle<()>> {
    let app = axum::Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            get(
                |UrlPath(token): UrlPath<String>, State(tokens): State<Tokens>| async move {
                    let tokens = tokens.lock().unwrap();
                    match tokens.iter().find(|(t, _)| *t == token) {
                        Some((_, answer)) => (StatusCode::OK, answer.clone()),
                        None => (StatusCode::NOT_FOUND, String::new()),
                    }
                },
            ),
        )
        .with_state(tokens);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("listening on {address} for the HTTP-01 challenges"))?;
    info!(%address, "answering the HTTP-01 challenges");
    Ok(tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    }))
}

/// Write `bytes` to `path`, readable by its owner only, replacing the file whole.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    put_in_place(&write_beside(path, bytes)?, path)
}

fn put_in_place(temporary: &Path, path: &Path) -> Result<()> {
    std::fs::rename(temporary, path).with_context(|| format!("writing {}", path.display()))
}

/// Write `bytes` to a file beside `path`, readable by its owner only, synced: to be renamed to
/// `path`.
fn write_beside(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let temporary = path.with_extension("new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("writing {}", temporary.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(temporary)
}

/// A name as certificates are compared on it: an IP address in its canonical form (an IPv6 one
/// is written many ways), a DNS name in lower case (RFC 4343).
fn name_of(name: &str) -> String {
    match name.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => name.to_ascii_lowercase(),
    }
}

/// An IP address from its bytes in a certificate.
fn ip_of(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-signed certificate for `names`, and its key, in PEM.
    fn certificate(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let certificate = rcgen::CertificateParams::new(names)
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    /// The certificate in place is kept only if it is for every name asked, whatever their
    /// spelling, and its key is beside it.
    #[test]
    fn the_certificate_in_place_is_checked() {
        let dir = std::env::temp_dir().join(format!("qdw-acme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let acme = Acme {
            directory: String::new(),
            domains: vec!["Host.Example".into(), "2001:DB8:0::1".into()],
            contact: Vec::new(),
            eab: None,
            ca_bundle: None,
            http: "127.0.0.1:0".parse().unwrap(),
            dir: dir.clone(),
            certificate: dir.join("cert.pem"),
            key: dir.join("key.pem"),
        };
        let place = |(certificate, key): &(String, String)| {
            std::fs::write(&acme.certificate, certificate).unwrap();
            std::fs::write(&acme.key, key).unwrap();
        };
        let good = certificate(&["host.example", "2001:db8::1"]);
        place(&good);
        assert!(acme.due() > SystemTime::now());
        assert!(acme.usable());
        // Another key beside it.
        let other = certificate(&["host.example", "2001:db8::1"]);
        place(&(good.0.clone(), other.1));
        assert_eq!(acme.due(), SystemTime::UNIX_EPOCH);
        assert!(!acme.usable());
        // A name missing.
        place(&certificate(&["host.example"]));
        assert_eq!(acme.due(), SystemTime::UNIX_EPOCH);
        assert!(!acme.usable());
        // Expired: due, and of no use to start with.
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(vec!["host.example".into(), "2001:db8::1".into()])
                .unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2020, 4, 1);
        place(&(params.self_signed(&key).unwrap().pem(), key.serialize_pem()));
        assert!(acme.due() < SystemTime::now());
        assert!(!acme.usable());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_are_compared_in_one_form() {
        assert_eq!(name_of("2001:DB8:0::1"), "2001:db8::1");
        assert_eq!(name_of("Host.Example"), "host.example");
        assert_eq!(name_of("192.0.2.1"), "192.0.2.1");
    }
}
