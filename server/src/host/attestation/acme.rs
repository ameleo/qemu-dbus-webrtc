// SPDX-License-Identifier: MPL-2.0

//! An ACME client (RFC 8555) for the host's certificate from its broker: flattened JWS signed
//! with an ECDSA P-256 account key (ES256), the account bound by the broker's enrollment token
//! (an external account binding, 7.3.4), and the requests of the broker's attestation key
//! service (`attest/v1/ak/begin` and `finish`), signed by the account as any other.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};

/// The broker's answers must come within this.
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    http: reqwest::Client,
    directory: Value,
    key: EcdsaKeyPair,
    /// The account's URL, once there is one: what signs by `kid`.
    account: Option<String>,
    nonce: Option<String>,
}

/// An answer: its status, its Location, its body.
pub struct Answer {
    pub status: u16,
    pub location: Option<String>,
    pub body: String,
}

impl Answer {
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.body).with_context(|| format!("the answer {}", self.body))
    }

    /// The problem document of an error (RFC 7807), as a message.
    fn problem(&self) -> String {
        match serde_json::from_str::<Value>(&self.body) {
            Ok(problem) => format!(
                "{} {}: {}",
                self.status,
                problem["type"].as_str().unwrap_or_default(),
                problem["detail"].as_str().unwrap_or_default()
            ),
            Err(_) => format!("{} {}", self.status, self.body),
        }
    }
}

/// The account key: PKCS#8 in `path`, made there the first time (mode 0600).
pub fn account_key(path: &Path) -> Result<EcdsaKeyPair> {
    let random = SystemRandom::new();
    let pkcs8 = match std::fs::read(path) {
        Ok(pkcs8) => pkcs8,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &random)
                .map_err(|_| anyhow!("generating the ACME account key"))?;
            super::write_private(path, pkcs8.as_ref())?;
            pkcs8.as_ref().to_vec()
        }
        Err(error) => return Err(error).with_context(|| format!("{}", path.display())),
    };
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &random)
        .map_err(|_| anyhow!("{}: not an ECDSA P-256 key", path.display()))
}

impl Client {
    /// Reads the directory. `roots`, PEM, authenticate the broker instead of the system's.
    pub async fn new(directory: &str, roots: Option<&[u8]>, key: EcdsaKeyPair) -> Result<Client> {
        let mut http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT);
        if let Some(roots) = roots {
            http = http.tls_built_in_root_certs(false);
            for certificate in reqwest::Certificate::from_pem_bundle(roots)? {
                http = http.add_root_certificate(certificate);
            }
        }
        let http = http.build()?;
        let directory: Value = serde_json::from_str(
            &http
                .get(directory)
                .send()
                .await
                .with_context(|| format!("the ACME directory {directory}"))?
                .error_for_status()?
                .text()
                .await?,
        )
        .context("the ACME directory")?;
        Ok(Client {
            http,
            directory,
            key,
            account: None,
            nonce: None,
        })
    }

    pub fn url(&self, resource: &str) -> Result<String> {
        self.directory[resource]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("the ACME directory has no {resource}"))
    }

    /// The account key as a JWK (RFC 7518 6.2.1).
    pub fn jwk(&self) -> Value {
        let point = self.key.public_key().as_ref();
        json!({
            "crv": "P-256",
            "kty": "EC",
            "x": B64.encode(&point[1..33]),
            "y": B64.encode(&point[33..65]),
        })
    }

    /// The JWK thumbprint (RFC 7638): SHA-256 of its required members, in lexicographic order
    /// and without whitespace, as `jwk` writes them.
    pub fn thumbprint(&self) -> String {
        let point = self.key.public_key().as_ref();
        let canonical = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            B64.encode(&point[1..33]),
            B64.encode(&point[33..65])
        );
        B64.encode(ring::digest::digest(
            &ring::digest::SHA256,
            canonical.as_bytes(),
        ))
    }

    /// The key authorization of a challenge (RFC 8555 8.1).
    pub fn key_authorization(&self, token: &str) -> String {
        format!("{token}.{}", self.thumbprint())
    }

    async fn fresh_nonce(&mut self) -> Result<String> {
        if let Some(nonce) = self.nonce.take() {
            return Ok(nonce);
        }
        let response = self.http.head(self.url("newNonce")?).send().await?;
        response
            .headers()
            .get("replay-nonce")
            .and_then(|nonce| nonce.to_str().ok())
            .map(str::to_owned)
            .context("no Replay-Nonce from the ACME server")
    }

    /// A request signed with the account key (RFC 8555 6.2): by `kid` once there is an account,
    /// by `jwk` before. `None` for a POST-as-GET. A badNonce is tried again once (6.5).
    pub async fn post(&mut self, url: &str, payload: Option<&Value>) -> Result<Answer> {
        for attempt in 0..2 {
            let nonce = self.fresh_nonce().await?;
            let mut protected = json!({ "alg": "ES256", "nonce": nonce, "url": url });
            match &self.account {
                Some(account) => protected["kid"] = json!(account),
                None => protected["jwk"] = self.jwk(),
            }
            let protected = B64.encode(protected.to_string());
            let payload = payload.map_or_else(String::new, |p| B64.encode(p.to_string()));
            let signature = self
                .key
                .sign(
                    &SystemRandom::new(),
                    format!("{protected}.{payload}").as_bytes(),
                )
                .map_err(|_| anyhow!("signing an ACME request"))?;
            let body = json!({
                "protected": protected,
                "payload": payload,
                "signature": B64.encode(signature.as_ref()),
            });
            let response = self
                .http
                .post(url)
                .header("content-type", "application/jose+json")
                .body(body.to_string())
                .send()
                .await
                .with_context(|| format!("ACME {url}"))?;
            let headers = response.headers();
            self.nonce = headers
                .get("replay-nonce")
                .and_then(|nonce| nonce.to_str().ok())
                .map(str::to_owned);
            let location = headers
                .get("location")
                .and_then(|location| location.to_str().ok())
                .map(str::to_owned);
            let answer = Answer {
                status: response.status().as_u16(),
                location,
                body: response.text().await?,
            };
            let bad_nonce =
                answer.status == 400 && answer.body.contains("urn:ietf:params:acme:error:badNonce");
            if bad_nonce && attempt == 0 {
                continue;
            }
            return Ok(answer);
        }
        unreachable!()
    }

    /// A request that must succeed: its answer, or the problem as an error.
    pub async fn expect(&mut self, url: &str, payload: Option<&Value>) -> Result<Answer> {
        let answer = self.post(url, payload).await?;
        if !(200..300).contains(&answer.status) {
            bail!("{url}: {}", answer.problem());
        }
        Ok(answer)
    }

    /// The account: found again by its key, or created with the enrollment token (`kid`, `key`)
    /// when there is one.
    pub async fn account(&mut self, binding: Option<(&str, &[u8])>) -> Result<()> {
        let url = self.url("newAccount")?;
        let existing = self
            .post(&url, Some(&json!({ "onlyReturnExisting": true })))
            .await?;
        if existing.status == 200 {
            self.account = Some(existing.location.context("an account without its URL")?);
            return Ok(());
        }
        let Some((kid, key)) = binding else {
            bail!(
                "no ACME account for this key, and no enrollment token: {}",
                existing.problem()
            );
        };
        let protected = B64.encode(json!({ "alg": "HS256", "kid": kid, "url": url }).to_string());
        let payload = B64.encode(self.jwk().to_string());
        let mac = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key),
            format!("{protected}.{payload}").as_bytes(),
        );
        let binding = json!({
            "protected": protected,
            "payload": payload,
            "signature": B64.encode(mac.as_ref()),
        });
        let created = self
            .expect(
                &url,
                Some(&json!({
                    "termsOfServiceAgreed": true,
                    "externalAccountBinding": binding,
                })),
            )
            .await?;
        ensure!(
            created.status == 201,
            "the account was not created: {}",
            created.status
        );
        self.account = Some(created.location.context("an account without its URL")?);
        Ok(())
    }

    /// The URL of a resource of the broker's attestation key service, next to newAccount.
    pub fn ak_url(&self, step: &str) -> Result<String> {
        let new_account = self.url("newAccount")?;
        let base = new_account
            .strip_suffix("/new-account")
            .context("an ACME server whose newAccount is not <base>/new-account")?;
        Ok(format!("{base}/attest/v1/ak/{step}"))
    }
}
