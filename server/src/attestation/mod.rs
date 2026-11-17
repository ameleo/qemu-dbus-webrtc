// SPDX-License-Identifier: MPL-2.0

//! The host's certificate to its broker, from the broker's ACME server (RFC 8555): its key in
//! its TPM, proven by device-attest-01 (draft-ietf-acme-device-attest-10) in the WebAuthn
//! "tpm" format; or, for a host the broker enrolled without a TPM, a key in a file and its
//! account alone.
//!
//! What it keeps, in its state directory (mode 0700, files 0600): the ACME account key
//! (`account.pk8`), the attestation key's blobs and certificate (`ak.blobs`, `ak.der`), the
//! host's key (`host-key.blobs` in the TPM, `host-key.pk8` without one) and its certificate
//! with the authority's (`host.pem`).

pub mod acme;
pub mod tpm;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD as B64};
use serde_json::{Value, json};
use tracing::info;

use acme::Client;
use tpm::{Blobs, Tpm};

/// How the host gets its certificate.
pub struct Settings {
    /// The broker's ACME directory.
    pub directory: String,
    /// The host's identifier, as the broker names it.
    pub host_id: String,
    /// The roots that authenticate the broker, PEM, instead of the system's.
    pub roots: Option<Vec<u8>>,
    pub state: PathBuf,
    /// The TPM's TCTI ("device:/dev/tpmrm0"); none for a host without a TPM.
    pub tpm: Option<String>,
    /// The enrollment token, the first time: its key identifier and HMAC key.
    pub enrollment: Option<(String, Vec<u8>)>,
}

/// Writes a private file: mode 0600, made whole then renamed.
pub fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let partial = path.with_extension("partial");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("{}", partial.display()))?;
    file.write_all(content)?;
    file.sync_all()?;
    std::fs::rename(&partial, path).with_context(|| format!("{}", path.display()))?;
    Ok(())
}

/// Gets the host's certificate, writing it to `host.pem` in the state directory.
pub async fn enroll(settings: &Settings) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&settings.state)
        .with_context(|| format!("{}", settings.state.display()))?;
    let state = &settings.state;
    let key = acme::account_key(&state.join("account.pk8"))?;
    let mut client = Client::new(&settings.directory, settings.roots.as_deref(), key).await?;
    let enrollment = settings
        .enrollment
        .as_ref()
        .map(|(kid, key)| (kid.as_str(), key.as_slice()));
    client.account(enrollment).await?;
    info!(directory = settings.directory, "ACME account");
    match &settings.tpm {
        Some(tcti) => enroll_with_tpm(settings, &mut client, tcti).await,
        None => enroll_without_tpm(settings, &mut client).await,
    }
}

/// The order for the host: its URL and its object.
async fn new_order(client: &mut Client, host: &str) -> Result<(String, Value)> {
    let url = client.url("newOrder")?;
    let created = client
        .expect(
            &url,
            Some(&json!({ "identifiers": [{ "type": "permanent-identifier", "value": host }] })),
        )
        .await?;
    let location = created
        .location
        .clone()
        .context("an order without its URL")?;
    Ok((location, created.json()?))
}

/// Reads the order until it leaves `from` (RFC 8555 7.1.6), a second apart, for half a minute.
async fn wait_order(client: &mut Client, url: &str, from: &str) -> Result<Value> {
    for _ in 0..30 {
        let order = client.expect(url, None).await?.json()?;
        if order["status"] != from {
            return Ok(order);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("the order stayed {from}")
}

/// Finalizes the order with `csr` (DER), and writes the certificate and its chain.
async fn finalize(
    client: &mut Client,
    url: &str,
    order: &Value,
    csr: &[u8],
    state: &Path,
) -> Result<()> {
    let finalize = order["finalize"]
        .as_str()
        .context("an order without its finalize URL")?;
    client
        .expect(finalize, Some(&json!({ "csr": B64.encode(csr) })))
        .await?;
    let order = wait_order(client, url, "processing").await?;
    ensure!(
        order["status"] == "valid",
        "the order is {}",
        order["status"]
    );
    let certificate = order["certificate"]
        .as_str()
        .context("a valid order without its certificate")?;
    let chain = client.expect(certificate, None).await?.body;
    ensure!(
        chain.contains("-----BEGIN CERTIFICATE-----"),
        "no certificate in the answer"
    );
    write_private(&state.join("host.pem"), chain.as_bytes())?;
    info!(path = %state.join("host.pem").display(), "host certificate");
    Ok(())
}

async fn enroll_without_tpm(settings: &Settings, client: &mut Client) -> Result<()> {
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    let state = &settings.state;
    let (url, order) = new_order(client, &settings.host_id).await?;
    ensure!(
        order["status"] == "ready",
        "the order is {}: the broker enrolled this host with a TPM ([broker] tpm)",
        order["status"]
    );
    // A new key for each certificate, in a file.
    let random = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random)
        .map_err(|_| anyhow::anyhow!("generating the host's key"))?;
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
        .map_err(|_| anyhow::anyhow!("the host's key"))?;
    let csr = csr::request(&settings.host_id, key.public_key().as_ref(), |info| {
        let signature = key
            .sign(&random, info)
            .map_err(|_| anyhow::anyhow!("signing the certificate request"))?;
        Ok(signature.as_ref().to_vec())
    })?;
    finalize(client, &url, &order, &csr, state).await?;
    write_private(&state.join("host-key.pk8"), pkcs8.as_ref())?;
    Ok(())
}

async fn enroll_with_tpm(settings: &Settings, client: &mut Client, tcti: &str) -> Result<()> {
    let state = &settings.state;
    let mut tpm = Tpm::open(tcti)?;
    let (ak, ak_certificate) = attestation_key(&mut tpm, client, state).await?;

    let (url, order) = new_order(client, &settings.host_id).await?;
    let authorization = order["authorizations"][0]
        .as_str()
        .context("an order without its authorization")?;
    let authorization = client.expect(authorization, None).await?.json()?;
    let challenge = authorization["challenges"]
        .as_array()
        .and_then(|all| all.iter().find(|c| c["type"] == "device-attest-01"))
        .context("no device-attest-01 challenge: the broker enrolled this host without a TPM")?
        .clone();
    let token = challenge["token"]
        .as_str()
        .context("a challenge without its token")?;
    let challenge_url = challenge["url"]
        .as_str()
        .context("a challenge without its URL")?;

    // A new key for each certificate, certified by the AK for the challenge's key
    // authorization: its SHA-256, ES256's digest (WebAuthn 8.3, draft 5).
    let srk = tpm.srk()?;
    let host_key = tpm.create_host_key(srk);
    let host_key = match host_key {
        Ok(key) => key,
        Err(error) => {
            tpm.flush(srk);
            return Err(error);
        }
    };
    let loaded = tpm.load(srk, &host_key);
    tpm.flush(srk);
    let key_handle = loaded?;
    let ek = tpm.endorsement()?;
    let ak_handle = tpm.load_ak(ek.handle, &ak);
    tpm.flush(ek.handle);
    let ak_handle = ak_handle?;
    let key_authorization = client.key_authorization(token);
    let extra_data = ring::digest::digest(&ring::digest::SHA256, key_authorization.as_bytes());
    let certified = tpm.certify_key(key_handle, ak_handle, extra_data.as_ref());
    tpm.flush(ak_handle);
    let (cert_info, signature) = certified?;
    let pub_area = {
        use tss_esapi::traits::Marshall;
        host_key.public.marshall()?
    };
    let att_obj = attestation_object(&ak_certificate, &signature, &cert_info, &pub_area)?;
    client
        .expect(
            challenge_url,
            Some(&json!({ "attObj": B64.encode(&att_obj) })),
        )
        .await?;
    let order = wait_order(client, &url, "pending").await?;
    ensure!(
        order["status"] == "ready",
        "the order is {}",
        order["status"]
    );

    let point = Tpm::point(&host_key.public)?;
    let csr = csr::request(&settings.host_id, &point, |info| {
        let digest: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, info)
            .as_ref()
            .try_into()
            .expect("32 bytes");
        let (r, s) = tpm.sign(key_handle, &digest)?;
        Ok(csr::ecdsa_signature(&r, &s))
    });
    let csr = match csr {
        Ok(csr) => csr,
        Err(error) => {
            tpm.flush(key_handle);
            return Err(error);
        }
    };
    tpm.flush(key_handle);
    finalize(client, &url, &order, &csr, state).await?;
    write_private(&state.join("host-key.blobs"), &host_key.to_bytes()?)?;
    Ok(())
}

/// The attestation key, and its certificate from the broker: kept from a previous enrollment,
/// or made and certified now.
async fn attestation_key(
    tpm: &mut Tpm,
    client: &mut Client,
    state: &Path,
) -> Result<(Blobs, Vec<u8>)> {
    let (blobs_path, certificate_path) = (state.join("ak.blobs"), state.join("ak.der"));
    if let (Ok(blobs), Ok(certificate)) =
        (std::fs::read(&blobs_path), std::fs::read(&certificate_path))
    {
        return Ok((Blobs::from_bytes(&blobs)?, certificate));
    }
    let ek = tpm.endorsement()?;
    let result = certify_attestation_key(tpm, client, &ek).await;
    tpm.flush(ek.handle);
    let (ak, certificate) = result?;
    write_private(&blobs_path, &ak.to_bytes()?)?;
    write_private(&certificate_path, &certificate)?;
    info!("attestation key certified by the broker");
    Ok((ak, certificate))
}

async fn certify_attestation_key(
    tpm: &mut Tpm,
    client: &mut Client,
    ek: &tpm::Endorsement,
) -> Result<(Blobs, Vec<u8>)> {
    use tss_esapi::traits::Marshall;
    let ak = tpm.create_ak(ek.handle)?;
    let begin = client
        .expect(
            &client.ak_url("begin")?,
            Some(&json!({
                "ek_certificate": STANDARD.encode(&ek.certificate),
                "ek_chain": ek.chain.iter().map(|der| STANDARD.encode(der)).collect::<Vec<_>>(),
                "ak_public": STANDARD.encode(tpm::tpm2b(&ak.public.marshall()?)),
            })),
        )
        .await?
        .json()?;
    let field = |name: &str| -> Result<Vec<u8>> {
        let value = begin[name]
            .as_str()
            .with_context(|| format!("ak/begin without {name}"))?;
        Ok(STANDARD.decode(value)?)
    };
    let ak_handle = tpm.load_ak(ek.handle, &ak)?;
    let certified = tpm.certify_ak(
        ek.handle,
        ak_handle,
        &field("import_public")?,
        &field("import_duplicate")?,
        &field("import_seed")?,
        &field("nonce")?,
    );
    tpm.flush(ak_handle);
    let (certify_info, certify_signature) = certified?;
    let finish = client
        .expect(
            &client.ak_url("finish")?,
            Some(&json!({
                "session": begin["session"],
                "certify_info": STANDARD.encode(certify_info),
                "certify_signature": STANDARD.encode(certify_signature),
            })),
        )
        .await?
        .json()?;
    let certificate = STANDARD.decode(
        finish["ak_certificate"]
            .as_str()
            .context("ak/finish without ak_certificate")?,
    )?;
    Ok((ak, certificate))
}

/// The attestation object of device-attest-01 (draft 5; WebAuthn 6.5.4 and 8.3): format "tpm",
/// its statement, no authData.
fn attestation_object(
    ak_certificate: &[u8],
    sig: &[u8],
    cert_info: &[u8],
    pub_area: &[u8],
) -> Result<Vec<u8>> {
    use ciborium::value::Value as Cbor;
    let statement = Cbor::Map(vec![
        (Cbor::Text("ver".into()), Cbor::Text("2.0".into())),
        // COSE ES256 (RFC 9053).
        (Cbor::Text("alg".into()), Cbor::Integer((-7).into())),
        (
            Cbor::Text("x5c".into()),
            Cbor::Array(vec![Cbor::Bytes(ak_certificate.to_vec())]),
        ),
        (Cbor::Text("sig".into()), Cbor::Bytes(sig.to_vec())),
        (
            Cbor::Text("certInfo".into()),
            Cbor::Bytes(cert_info.to_vec()),
        ),
        (Cbor::Text("pubArea".into()), Cbor::Bytes(pub_area.to_vec())),
    ]);
    let object = Cbor::Map(vec![
        (Cbor::Text("fmt".into()), Cbor::Text("tpm".into())),
        (Cbor::Text("attStmt".into()), statement),
    ]);
    let mut bytes = Vec::new();
    ciborium::ser::into_writer(&object, &mut bytes)?;
    Ok(bytes)
}

/// PKCS#10 certificate requests (RFC 2986) for an ECDSA P-256 key, in DER (X.690).
mod csr {
    use anyhow::Result;

    /// A TLV: `tag`, its length (short or long form), `content`.
    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let length = content.len();
        if length < 0x80 {
            out.push(length as u8);
        } else {
            let bytes: Vec<u8> = length
                .to_be_bytes()
                .into_iter()
                .skip_while(|b| *b == 0)
                .collect();
            out.push(0x80 | bytes.len() as u8);
            out.extend(bytes);
        }
        out.extend_from_slice(content);
        out
    }

    fn sequence(parts: &[&[u8]]) -> Vec<u8> {
        tlv(0x30, &parts.concat())
    }

    /// A positive INTEGER from big-endian bytes: no leading zeros, but one if the high bit is set.
    fn integer(bytes: &[u8]) -> Vec<u8> {
        let trimmed: Vec<u8> = bytes.iter().copied().skip_while(|b| *b == 0).collect();
        let mut content = if trimmed.is_empty() { vec![0] } else { trimmed };
        if content[0] & 0x80 != 0 {
            content.insert(0, 0);
        }
        tlv(0x02, &content)
    }

    /// id-ecPublicKey, prime256v1, ecdsa-with-SHA256 (RFC 5480, RFC 5758), commonName (X.520).
    const ID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    const PRIME256V1: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
    const ECDSA_WITH_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
    const COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];

    /// An ECDSA-Sig-Value (RFC 5480 2.2... RFC 3279 2.2.3): SEQUENCE { r, s }.
    pub fn ecdsa_signature(r: &[u8], s: &[u8]) -> Vec<u8> {
        sequence(&[&integer(r), &integer(s)])
    }

    /// A request for `point` (an uncompressed P-256 point) named CN=`name`, its
    /// CertificationRequestInfo signed by `sign` (an ECDSA-Sig-Value in DER).
    pub fn request(
        name: &str,
        point: &[u8],
        sign: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let subject = sequence(&[&tlv(
            0x31,
            &sequence(&[COMMON_NAME, &tlv(0x0C, name.as_bytes())]),
        )]);
        let algorithm = sequence(&[ID_EC_PUBLIC_KEY, PRIME256V1]);
        let mut bits = vec![0];
        bits.extend_from_slice(point);
        let spki = sequence(&[&algorithm, &tlv(0x03, &bits)]);
        // version 0, the subject, its key, no attributes ([0] IMPLICIT SET OF, empty).
        let info = sequence(&[&integer(&[0]), &subject, &spki, &[0xA0, 0x00]]);
        let signature = sign(&info)?;
        let mut bits = vec![0];
        bits.extend_from_slice(&signature);
        Ok(sequence(&[
            &info,
            &sequence(&[ECDSA_WITH_SHA256]),
            &tlv(0x03, &bits),
        ]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn der() {
            assert_eq!(integer(&[0, 0, 5]), [0x02, 0x01, 0x05]);
            assert_eq!(integer(&[0x80]), [0x02, 0x02, 0x00, 0x80]);
            assert_eq!(integer(&[0]), [0x02, 0x01, 0x00]);
            assert_eq!(tlv(0x04, &[0; 200])[..3], [0x04, 0x81, 200]);
        }

        /// A request ring signs reads back with OpenSSL, its signature verified.
        #[test]
        fn openssl_reads_and_verifies_a_request() {
            use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
            let random = ring::rand::SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
                    .unwrap();
            let der = request("host-1", key.public_key().as_ref(), |info| {
                Ok(key.sign(&random, info).unwrap().as_ref().to_vec())
            })
            .unwrap();
            let Ok(mut openssl) = std::process::Command::new("openssl")
                .args(["req", "-inform", "DER", "-noout", "-verify", "-subject"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            else {
                eprintln!("skipped: no openssl");
                return;
            };
            use std::io::Write;
            openssl.stdin.take().unwrap().write_all(&der).unwrap();
            let output = openssl.wait_with_output().unwrap();
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.status.success(), "{text}");
            assert!(
                text.contains("verify OK")
                    || text.contains("Certificate request self-signature verify OK"),
                "{text}"
            );
            assert!(
                text.contains("CN=host-1") || text.contains("CN = host-1"),
                "{text}"
            );
        }
    }
}
