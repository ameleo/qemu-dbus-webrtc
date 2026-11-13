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
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The HTTP-01 challenges being answered: their tokens, and the answers.
type Tokens = Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// How long the authority has to validate the challenges, and to issue the certificate.
const VALIDATION_TIME: Duration = Duration::from_secs(120);
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

    /// The validity of the certificate in place, if it is for every name asked.
    fn lifetime(&self) -> Result<(SystemTime, SystemTime)> {
        let pem = std::fs::read(&self.certificate)
            .with_context(|| format!("reading {}", self.certificate.display()))?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&pem).context("not a PEM certificate")?;
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
                        x509_parser::extensions::GeneralName::DNSName(dns) => {
                            Some(dns.to_ascii_lowercase())
                        }
                        x509_parser::extensions::GeneralName::IPAddress(ip) => {
                            ip_of(ip).map(|ip| ip.to_string())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(missing) = self
            .domains
            .iter()
            .find(|d| !names.contains(&d.to_ascii_lowercase()))
        {
            bail!("the certificate in place is not for {missing}");
        }
        let validity = certificate.validity();
        let time = |t: x509_parser::time::ASN1Time| {
            SystemTime::UNIX_EPOCH + Duration::from_secs(t.timestamp().max(0) as u64)
        };
        Ok((time(validity.not_before), time(validity.not_after)))
    }

    /// Get a certificate from the authority, and write it with its key.
    pub async fn renew(&self) -> Result<()> {
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
            let server = serve_challenges(self.http, tokens.clone()).await?;
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
            server.abort();
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
        write_private(&self.key, key.as_bytes())?;
        write_private(&self.certificate, chain.as_bytes())?;
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
            Ok(()) => reload(),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "certificate not renewed: trying again in an hour");
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
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
    std::fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))
}

/// An IP address from its bytes in a certificate.
fn ip_of(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}
