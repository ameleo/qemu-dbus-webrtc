// SPDX-License-Identifier: MPL-2.0

//! The host's TPM, through the TCG's stack (tpm2-tss, its Enhanced System API): what a broker's
//! ACME server asks to prove that the host's key lives in it (draft-ietf-acme-device-attest,
//! device-attest-01 in the WebAuthn "tpm" format).
//!
//! - The endorsement key (EK), from the TCG's default template, its certificate and the
//!   manufacturer's intermediates as the TPM keeps them in NV (TCG EK Credential Profile).
//! - An attestation key (AK) under the EK: a restricted ECDSA P-256 key. The broker certifies it
//!   once: it wraps an HMAC key to the EK, which only this TPM can import (TPM2_Import), and
//!   that key certifies the AK for the broker's nonce (TPM2_Certify).
//! - The host's key under the storage root key (SRK): an ECDSA P-256 signing key, fixed to this
//!   TPM, which the AK certifies for the challenge (TPM2_Certify), and which signs the host's
//!   certificate request and its TLS handshakes.
//!
//! Every key is transient: created again from its template (the primaries) or loaded from its
//! blobs (the AK and the host's key) when needed, and flushed after.

use std::str::FromStr;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use tss_esapi::Context;
use tss_esapi::abstraction::{AsymmetricAlgorithmSelection, ak, ek};
use tss_esapi::attributes::{ObjectAttributesBuilder, SessionAttributesBuilder};
use tss_esapi::constants::SessionType;
use tss_esapi::handles::{KeyHandle, ObjectHandle, SessionHandle};
use tss_esapi::interface_types::algorithm::{
    HashingAlgorithm, PublicAlgorithm, SignatureSchemeAlgorithm,
};
use tss_esapi::interface_types::ecc::EccCurve;
use tss_esapi::interface_types::resource_handles::Hierarchy;
use tss_esapi::interface_types::session_handles::PolicySession;
use tss_esapi::structures::{
    Data, Digest, EccPoint, EccScheme, EncryptedSecret, HashScheme, HashcheckTicket,
    KeyDerivationFunctionScheme, Private, Public, PublicBuilder, PublicEccParametersBuilder,
    Signature, SignatureScheme, SymmetricDefinition, SymmetricDefinitionObject,
};
use tss_esapi::tcti_ldr::TctiNameConf;
use tss_esapi::traits::{Marshall, UnMarshall};

/// The TCG's NV index of the EK certificate chain's first part (TCG EK Credential Profile,
/// "EK Certificate Chain": from 0x01C00100), DER certificates one after the other.
const EK_CHAIN_INDEX: u32 = 0x01C0_0100;

/// The TPM, open.
pub struct Tpm {
    context: Context,
}

/// The endorsement key, loaded, and what vouches for it.
pub struct Endorsement {
    pub handle: KeyHandle,
    /// The EK certificate, DER.
    pub certificate: Vec<u8>,
    /// The manufacturer's intermediates the TPM keeps, DER.
    pub chain: Vec<Vec<u8>>,
}

/// A key's blobs, as TPM2_Create gives them: to load it again under its parent.
#[derive(Clone)]
pub struct Blobs {
    pub public: Public,
    pub private: Private,
}

impl Blobs {
    /// TPMT_PUBLIC then TPM2B_PRIVATE, as kept in a file.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let public = self.public.marshall()?;
        let mut bytes = (public.len() as u16).to_be_bytes().to_vec();
        bytes.extend_from_slice(&public);
        let private = self.private.value();
        bytes.extend_from_slice(&(private.len() as u16).to_be_bytes());
        bytes.extend_from_slice(private);
        Ok(bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Blobs> {
        let (public, rest) = sized(bytes)?;
        let (private, rest) = sized(rest)?;
        ensure!(rest.is_empty(), "trailing bytes after a key's blobs");
        Ok(Blobs {
            public: Public::unmarshall(public)?,
            private: Private::try_from(private.to_vec())?,
        })
    }
}

/// The content of a size-prefixed buffer (a TPM2B), and what follows it.
fn sized(bytes: &[u8]) -> Result<(&[u8], &[u8])> {
    let (size, rest) = bytes
        .split_first_chunk::<2>()
        .ok_or_else(|| anyhow!("a truncated buffer"))?;
    let size = usize::from(u16::from_be_bytes(*size));
    ensure!(rest.len() >= size, "a truncated buffer");
    Ok(rest.split_at(size))
}

/// A TPM2B: the content, size first.
pub fn tpm2b(content: &[u8]) -> Vec<u8> {
    let mut bytes = (content.len() as u16).to_be_bytes().to_vec();
    bytes.extend_from_slice(content);
    bytes
}

impl Tpm {
    /// Opens the TPM by its TCTI: "device:/dev/tpmrm0" (the kernel's resource manager), or
    /// "swtpm:host=127.0.0.1,port=2321" for a software TPM.
    pub fn open(tcti: &str) -> Result<Tpm> {
        let tcti = TctiNameConf::from_str(tcti).with_context(|| format!("the TPM {tcti:?}"))?;
        Ok(Tpm {
            context: Context::new(tcti).context("opening the TPM")?,
        })
    }

    /// The EK from the TCG's default template, ECC P-256 when the TPM has its certificate,
    /// else RSA 2048; its certificate and chain.
    pub fn endorsement(&mut self) -> Result<Endorsement> {
        let ctx = &mut self.context;
        for alg in [
            AsymmetricAlgorithmSelection::Ecc(EccCurve::NistP256),
            AsymmetricAlgorithmSelection::Rsa(
                tss_esapi::interface_types::key_bits::RsaKeyBits::Rsa2048,
            ),
        ] {
            let Ok(certificate) = ek::retrieve_ek_pubcert(ctx, alg) else {
                continue;
            };
            let certificate = first_der(&certificate)?.to_vec();
            let handle = ek::create_ek_object_2(ctx, alg, None).context("creating the EK")?;
            let chain = match read_nv(ctx, EK_CHAIN_INDEX) {
                Ok(bytes) => split_der(&bytes)?,
                Err(_) => Vec::new(),
            };
            return Ok(Endorsement {
                handle,
                certificate,
                chain,
            });
        }
        bail!("the TPM keeps no EK certificate (ECC P-256 nor RSA 2048)")
    }

    /// A new attestation key under the EK: restricted, ECDSA P-256 with SHA-256.
    pub fn create_ak(&mut self, ek: KeyHandle) -> Result<Blobs> {
        let created = ak::create_ak_2(
            &mut self.context,
            ek,
            HashingAlgorithm::Sha256,
            AsymmetricAlgorithmSelection::Ecc(EccCurve::NistP256),
            SignatureSchemeAlgorithm::EcDsa,
            None,
            None,
        )
        .context("creating the attestation key")?;
        Ok(Blobs {
            public: created.out_public,
            private: created.out_private,
        })
    }

    pub fn load_ak(&mut self, ek: KeyHandle, ak: &Blobs) -> Result<KeyHandle> {
        ak::load_ak(
            &mut self.context,
            ek,
            None,
            ak.private.clone(),
            ak.public.clone(),
        )
        .context("loading the attestation key")
    }

    /// Runs `f` in a policy session that satisfies the EK's policy: PolicySecret on the
    /// endorsement hierarchy (TCG EK Credential Profile, the default templates' policy A).
    fn with_ek_policy<T>(
        &mut self,
        ek: KeyHandle,
        f: impl FnOnce(&mut Context) -> tss_esapi::Result<T>,
    ) -> Result<T> {
        let ctx = &mut self.context;
        let (public, _, _) = ctx.read_public(ek)?;
        let hash = public.name_hashing_algorithm();
        let session = ctx
            .start_auth_session(
                None,
                None,
                None,
                SessionType::Policy,
                SymmetricDefinition::AES_128_CFB,
                hash,
            )?
            .ok_or_else(|| anyhow!("no session from the TPM"))?;
        let (attributes, mask) = SessionAttributesBuilder::new()
            .with_decrypt(true)
            .with_encrypt(true)
            .build();
        ctx.tr_sess_set_attributes(session, attributes, mask)?;
        let result =
            ctx.execute_with_temporary_object(SessionHandle::from(session).into(), |ctx, _| {
                ctx.execute_with_nullauth_session(|ctx| {
                    ctx.policy_secret(
                        PolicySession::try_from(session)?,
                        tss_esapi::handles::AuthHandle::Endorsement,
                        Default::default(),
                        Default::default(),
                        Default::default(),
                        None,
                    )
                })?;
                ctx.execute_with_session(Some(session), f)
            })?;
        Ok(result)
    }

    /// The broker's challenge for the AK: imports its HMAC key under the EK (TPM2_Import, an
    /// outer wrapper only), loads it, and has it certify the AK for `nonce` (TPM2_Certify).
    /// Returns the TPMS_ATTEST and the TPMT_SIGNATURE.
    pub fn certify_ak(
        &mut self,
        ek: KeyHandle,
        ak: KeyHandle,
        import_public: &[u8],
        import_duplicate: &[u8],
        import_seed: &[u8],
        nonce: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let (public, rest) = sized(import_public)?;
        ensure!(rest.is_empty(), "import_public is not a TPM2B_PUBLIC");
        let public = Public::unmarshall(public)?;
        let (duplicate, rest) = sized(import_duplicate)?;
        ensure!(rest.is_empty(), "import_duplicate is not a TPM2B_PRIVATE");
        let (seed, rest) = sized(import_seed)?;
        ensure!(
            rest.is_empty(),
            "import_seed is not a TPM2B_ENCRYPTED_SECRET"
        );
        let duplicate = Private::try_from(duplicate.to_vec())?;
        let seed = EncryptedSecret::try_from(seed.to_vec())?;
        let imported_public = public.clone();
        let private = self
            .with_ek_policy(ek, |ctx| {
                ctx.import(
                    ek.into(),
                    None,
                    imported_public,
                    duplicate,
                    seed,
                    SymmetricDefinitionObject::Null,
                )
            })
            .context("importing the broker's key (TPM2_Import)")?;
        let hmac = self
            .with_ek_policy(ek, |ctx| ctx.load(ek, private, public))
            .context("loading the broker's key")?;
        // Two authorizations: the certified object's and the signing key's (TPM 2.0 Part 3,
        // TPM2_Certify), empty passwords both.
        let certified = self.context.execute_with_sessions(
            (
                Some(tss_esapi::interface_types::session_handles::AuthSession::Password),
                Some(tss_esapi::interface_types::session_handles::AuthSession::Password),
                None,
            ),
            |ctx| {
                ctx.certify(
                    ObjectHandle::from(ak),
                    hmac,
                    Data::try_from(nonce.to_vec())?,
                    SignatureScheme::Null,
                )
            },
        );
        self.flush(hmac);
        let (attest, signature) =
            certified.context("certifying the attestation key (TPM2_Certify)")?;
        Ok((attest.marshall()?, signature.marshall()?))
    }

    /// The storage root key, from the TCG's ECC P-256 template (TCG TPM v2.0 Provisioning
    /// Guidance, the SRK template): the same key each time, the hierarchy's seed unchanged.
    pub fn srk(&mut self) -> Result<KeyHandle> {
        let attributes = ObjectAttributesBuilder::new()
            .with_fixed_tpm(true)
            .with_fixed_parent(true)
            .with_sensitive_data_origin(true)
            .with_user_with_auth(true)
            .with_no_da(true)
            .with_restricted(true)
            .with_decrypt(true)
            .build()?;
        let parameters = PublicEccParametersBuilder::new()
            .with_symmetric(SymmetricDefinitionObject::AES_128_CFB)
            .with_ecc_scheme(EccScheme::Null)
            .with_curve(EccCurve::NistP256)
            .with_key_derivation_function_scheme(KeyDerivationFunctionScheme::Null)
            .with_is_signing_key(false)
            .with_is_decryption_key(true)
            .with_restricted(true)
            .build()?;
        let public = PublicBuilder::new()
            .with_public_algorithm(PublicAlgorithm::Ecc)
            .with_name_hashing_algorithm(HashingAlgorithm::Sha256)
            .with_object_attributes(attributes)
            .with_ecc_parameters(parameters)
            .with_ecc_unique_identifier(EccPoint::default())
            .build()?;
        let created = self
            .context
            .execute_with_nullauth_session(|ctx| {
                ctx.create_primary(Hierarchy::Owner, public, None, None, None, None)
            })
            .context("creating the storage root key")?;
        Ok(created.key_handle)
    }

    /// A new key for the host under the SRK: ECDSA P-256 with SHA-256, signing only, fixed to
    /// this TPM and its parent, generated inside it.
    pub fn create_host_key(&mut self, srk: KeyHandle) -> Result<Blobs> {
        let attributes = ObjectAttributesBuilder::new()
            .with_fixed_tpm(true)
            .with_fixed_parent(true)
            .with_sensitive_data_origin(true)
            .with_user_with_auth(true)
            .with_sign_encrypt(true)
            .build()?;
        let parameters = PublicEccParametersBuilder::new()
            .with_symmetric(SymmetricDefinitionObject::Null)
            .with_ecc_scheme(EccScheme::EcDsa(HashScheme::new(HashingAlgorithm::Sha256)))
            .with_curve(EccCurve::NistP256)
            .with_key_derivation_function_scheme(KeyDerivationFunctionScheme::Null)
            .with_is_signing_key(true)
            .with_is_decryption_key(false)
            .with_restricted(false)
            .build()?;
        let public = PublicBuilder::new()
            .with_public_algorithm(PublicAlgorithm::Ecc)
            .with_name_hashing_algorithm(HashingAlgorithm::Sha256)
            .with_object_attributes(attributes)
            .with_ecc_parameters(parameters)
            .with_ecc_unique_identifier(EccPoint::default())
            .build()?;
        let created = self
            .context
            .execute_with_nullauth_session(|ctx| ctx.create(srk, public, None, None, None, None))
            .context("creating the host's key")?;
        Ok(Blobs {
            public: created.out_public,
            private: created.out_private,
        })
    }

    pub fn load(&mut self, parent: KeyHandle, key: &Blobs) -> Result<KeyHandle> {
        self.context
            .execute_with_nullauth_session(|ctx| {
                ctx.load(parent, key.private.clone(), key.public.clone())
            })
            .context("loading a key")
    }

    /// The AK certifies `key` for `extra_data` (TPM2_Certify): the TPMS_ATTEST and the
    /// TPMT_SIGNATURE of the WebAuthn "tpm" statement's certInfo and sig.
    pub fn certify_key(
        &mut self,
        key: KeyHandle,
        ak: KeyHandle,
        extra_data: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let (attest, signature) = self
            .context
            .execute_with_sessions(
                (
                    Some(tss_esapi::interface_types::session_handles::AuthSession::Password),
                    Some(tss_esapi::interface_types::session_handles::AuthSession::Password),
                    None,
                ),
                |ctx| {
                    ctx.certify(
                        ObjectHandle::from(key),
                        ak,
                        Data::try_from(extra_data.to_vec())?,
                        SignatureScheme::EcDsa {
                            hash_scheme: HashScheme::new(HashingAlgorithm::Sha256),
                        },
                    )
                },
            )
            .context("certifying the host's key (TPM2_Certify)")?;
        Ok((attest.marshall()?, signature.marshall()?))
    }

    /// An ECDSA signature by `key` of a SHA-256 `digest`: r and s, 32 bytes each.
    pub fn sign(&mut self, key: KeyHandle, digest: &[u8; 32]) -> Result<([u8; 32], [u8; 32])> {
        let signature = self
            .context
            .execute_with_nullauth_session(|ctx| {
                ctx.sign(
                    key,
                    Digest::try_from(digest.to_vec())?,
                    SignatureScheme::EcDsa {
                        hash_scheme: HashScheme::new(HashingAlgorithm::Sha256),
                    },
                    HashcheckTicket::try_from(tss_esapi::tss2_esys::TPMT_TK_HASHCHECK {
                        tag: tss_esapi::constants::tss::TPM2_ST_HASHCHECK,
                        hierarchy: tss_esapi::constants::tss::TPM2_RH_NULL,
                        digest: Default::default(),
                    })?,
                )
            })
            .context("signing with the host's key")?;
        let Signature::EcDsa(signature) = signature else {
            bail!("an ECDSA key gave another signature");
        };
        let fixed = |part: &[u8]| -> Result<[u8; 32]> {
            ensure!(
                part.len() <= 32,
                "a P-256 signature's part longer than 32 bytes"
            );
            let mut out = [0u8; 32];
            out[32 - part.len()..].copy_from_slice(part);
            Ok(out)
        };
        Ok((
            fixed(signature.signature_r().value())?,
            fixed(signature.signature_s().value())?,
        ))
    }

    /// The uncompressed point of an ECC P-256 key's public area: 0x04, x, y (SEC 1).
    pub fn point(public: &Public) -> Result<Vec<u8>> {
        let Public::Ecc { unique, .. } = public else {
            bail!("not an ECC key");
        };
        let mut point = vec![0x04];
        for coordinate in [unique.x().value(), unique.y().value()] {
            ensure!(
                coordinate.len() <= 32,
                "a P-256 coordinate longer than 32 bytes"
            );
            point.extend(std::iter::repeat_n(0, 32 - coordinate.len()));
            point.extend_from_slice(coordinate);
        }
        Ok(point)
    }

    pub fn flush(&mut self, handle: KeyHandle) {
        let _ = self.context.flush_context(handle.into());
    }
}

/// An NV index's whole content.
fn read_nv(ctx: &mut Context, index: u32) -> Result<Vec<u8>> {
    use tss_esapi::handles::{NvIndexTpmHandle, TpmHandle};
    use tss_esapi::interface_types::resource_handles::NvAuth;
    let index = NvIndexTpmHandle::new(index)?;
    let auth = ctx.execute_without_session(|ctx| {
        ctx.tr_from_tpm_public(TpmHandle::NvIndex(index))
            .map(|handle| NvAuth::NvIndex(handle.into()))
    })?;
    Ok(ctx.execute_with_nullauth_session(|ctx| {
        tss_esapi::abstraction::nv::read_full(ctx, auth, index)
    })?)
}

/// The length of the DER element at the start of `bytes`, header included.
fn der_length(bytes: &[u8]) -> Result<usize> {
    ensure!(bytes.len() >= 2, "a truncated DER element");
    let first = usize::from(bytes[1]);
    if first < 0x80 {
        return Ok(2 + first);
    }
    let count = first & 0x7f;
    ensure!(
        (1..=4).contains(&count) && bytes.len() >= 2 + count,
        "a DER length of an unexpected form"
    );
    let length = bytes[2..2 + count]
        .iter()
        .fold(0usize, |length, byte| (length << 8) | usize::from(*byte));
    Ok(2 + count + length)
}

/// The DER certificate at the start of an NV index's content: the index may be padded after it.
fn first_der(bytes: &[u8]) -> Result<&[u8]> {
    ensure!(bytes.first() == Some(&0x30), "not a DER certificate");
    let length = der_length(bytes)?;
    ensure!(length <= bytes.len(), "a truncated certificate");
    Ok(&bytes[..length])
}

/// The DER certificates one after the other of an NV index, padding (zeros or 0xFF) after them
/// left out.
fn split_der(mut bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut certificates = Vec::new();
    while bytes.first() == Some(&0x30) {
        let certificate = first_der(bytes)?;
        certificates.push(certificate.to_vec());
        bytes = &bytes[certificate.len()..];
    }
    Ok(certificates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_certificates_one_after_the_other() {
        // Two SEQUENCEs, short then long form, then padding.
        let mut bytes = vec![0x30, 0x02, 1, 2, 0x30, 0x81, 0x03, 7, 8, 9];
        bytes.extend([0xff; 4]);
        let split = split_der(&bytes).unwrap();
        assert_eq!(
            split,
            [vec![0x30, 0x02, 1, 2], vec![0x30, 0x81, 0x03, 7, 8, 9]]
        );
        assert!(split_der(&[0x30, 0x05, 1]).is_err());
        assert_eq!(first_der(&[0x30, 0x01, 5, 0, 0]).unwrap(), [0x30, 0x01, 5]);
    }

    #[test]
    fn sized_buffers() {
        assert_eq!(
            sized(&[0, 2, 7, 8, 9]).unwrap(),
            (&[7u8, 8][..], &[9u8][..])
        );
        assert!(sized(&[0, 3, 7]).is_err());
        assert_eq!(tpm2b(&[1, 2]), [0, 2, 1, 2]);
    }

    /// Against a TPM (`QDW_TEST_TPM`, a TCTI: "device:/dev/tpmrm0", "swtpm:port=2321"): the EK
    /// and its certificate, an AK created and loaded again from its blobs, the host's key
    /// certified by the AK for some data, and a signature by it that holds.
    #[test]
    fn against_a_tpm() {
        let Ok(tcti) = std::env::var("QDW_TEST_TPM") else {
            eprintln!("skipped: QDW_TEST_TPM, a TPM's TCTI, is not set");
            return;
        };
        let mut tpm = Tpm::open(&tcti).unwrap();
        let ek = tpm.endorsement().unwrap();
        assert_eq!(ek.certificate[0], 0x30);
        eprintln!(
            "EK certificate {} bytes, chain of {}",
            ek.certificate.len(),
            ek.chain.len()
        );
        let ak = tpm.create_ak(ek.handle).unwrap();
        let ak = Blobs::from_bytes(&ak.to_bytes().unwrap()).unwrap();
        let ak_handle = tpm.load_ak(ek.handle, &ak).unwrap();
        // A TPM holds few objects at once (Intel PTT: 3): the EK goes once the AK is loaded.
        tpm.flush(ek.handle);
        let srk = tpm.srk().unwrap();
        let key = tpm.create_host_key(srk).unwrap();
        let key_handle = tpm.load(srk, &key).unwrap();
        tpm.flush(srk);
        let (attest, signature) = tpm
            .certify_key(key_handle, ak_handle, b"extra data")
            .unwrap();
        // TPMS_ATTEST: TPM_GENERATED_VALUE first; a TPMT_SIGNATURE of ECDSA (0x0018).
        assert_eq!(attest[..4], [0xff, 0x54, 0x43, 0x47]);
        assert_eq!(signature[..2], [0x00, 0x18]);
        let digest: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, b"message")
            .as_ref()
            .try_into()
            .unwrap();
        let (r, s) = tpm.sign(key_handle, &digest).unwrap();
        let point = Tpm::point(&key.public).unwrap();
        let verifier = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            &point,
        );
        verifier.verify(b"message", &[r, s].concat()).unwrap();
        for handle in [key_handle, ak_handle] {
            tpm.flush(handle);
        }
    }
}
