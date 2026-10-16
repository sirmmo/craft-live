//! WOPI proof keys: craft-live signs every request it makes to a WOPI host, so the host can check
//! the request comes from the editor its discovery names (docs/design.md, "Proof keys").
//!
//! The signed data is, concatenated: the access token's length (4 bytes, big-endian) and its
//! UTF-8 bytes; the length and bytes of the request's absolute URL in upper case (query string
//! with `access_token` included); the length of the timestamp (8) and the timestamp (a 64-bit
//! big-endian integer, .NET ticks: 100 ns intervals since 0001-01-01 UTC). The signature is RSA
//! PKCS#1 v1.5 with SHA-256, in base64, in `X-WOPI-Proof` (current key) and `X-WOPI-ProofOld`
//! (previous key), with the timestamp in `X-WOPI-TimeStamp`.
//!
//! The discovery publishes both public keys: `modulus`/`exponent` as base64 of the unsigned
//! big-endian numbers, `value` as base64 of a CryptoAPI PUBLICKEYBLOB.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rand_core::OsRng;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{RandomizedSigner, SignatureEncoding, Verifier};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;

/// .NET ticks between 0001-01-01 and 1970-01-01.
const EPOCH_TICKS: u64 = 621_355_968_000_000_000;
/// How old a proof may be (the WOPI documentation's 20 minutes).
pub const MAX_AGE_TICKS: u64 = 20 * 60 * 10_000_000;

/// Now, in .NET ticks.
pub fn now_ticks() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let ticks = d.as_secs().saturating_mul(10_000_000).saturating_add(u64::from(d.subsec_nanos() / 100));
    EPOCH_TICKS.saturating_add(ticks)
}

/// The bytes that are signed.
pub fn signed_data(access_token: &str, url: &str, ticks: u64) -> Vec<u8> {
    let token = access_token.as_bytes();
    let url = url.to_uppercase();
    let url = url.as_bytes();
    let mut out = Vec::with_capacity(token.len() + url.len() + 20);
    out.extend_from_slice(&(token.len() as u32).to_be_bytes());
    out.extend_from_slice(token);
    out.extend_from_slice(&(url.len() as u32).to_be_bytes());
    out.extend_from_slice(url);
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&ticks.to_be_bytes());
    out
}

/// A public key as the discovery shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicParts {
    /// Base64 of the CryptoAPI PUBLICKEYBLOB.
    pub value: String,
    /// Base64 of the modulus (unsigned, big-endian).
    pub modulus: String,
    /// Base64 of the exponent (unsigned, big-endian).
    pub exponent: String,
}

/// The CryptoAPI PUBLICKEYBLOB of an RSA public key: PUBLICKEYSTRUC (bType 0x06, bVersion 2,
/// reserved 0, aiKeyAlg CALG_RSA_KEYX 0xA400), RSAPUBKEY ("RSA1", bit length, public exponent as
/// a 32-bit integer), then the modulus; all little-endian.
pub fn csp_blob(key: &RsaPublicKey) -> Vec<u8> {
    let modulus_be = key.n().to_bytes_be();
    let bits = (modulus_be.len() * 8) as u32;
    let exp = key.e().to_bytes_be();
    let mut exp32 = 0u32;
    for b in exp.iter().rev().take(4).rev() {
        exp32 = (exp32 << 8) | u32::from(*b);
    }
    let mut out = Vec::with_capacity(20 + modulus_be.len());
    out.extend_from_slice(&[0x06, 0x02, 0x00, 0x00]);
    out.extend_from_slice(&0x0000_a400u32.to_le_bytes());
    out.extend_from_slice(b"RSA1");
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(&exp32.to_le_bytes());
    out.extend(modulus_be.iter().rev());
    out
}

/// Reads a public key back from a PUBLICKEYBLOB (tests, and checking a host's view).
pub fn from_csp_blob(blob: &[u8]) -> Option<RsaPublicKey> {
    if blob.get(..2)? != [0x06, 0x02] || blob.get(8..12)? != b"RSA1" {
        return None;
    }
    let bits = u32::from_le_bytes(blob.get(12..16)?.try_into().ok()?);
    let exp = u32::from_le_bytes(blob.get(16..20)?.try_into().ok()?);
    let len = usize::try_from(bits / 8).ok()?;
    let modulus_le = blob.get(20..20usize.checked_add(len)?)?;
    let n = BigUint::from_bytes_le(modulus_le);
    RsaPublicKey::new(n, BigUint::from(exp)).ok()
}

pub fn public_parts(key: &RsaPublicKey) -> PublicParts {
    PublicParts { value: STANDARD.encode(csp_blob(key)), modulus: STANDARD.encode(key.n().to_bytes_be()), exponent: STANDARD.encode(key.e().to_bytes_be()) }
}

/// A public key from the discovery's `modulus` and `exponent`.
pub fn from_modulus_exponent(modulus: &str, exponent: &str) -> Option<RsaPublicKey> {
    let n = BigUint::from_bytes_be(&STANDARD.decode(modulus.trim()).ok()?);
    let e = BigUint::from_bytes_be(&STANDARD.decode(exponent.trim()).ok()?);
    RsaPublicKey::new(n, e).ok()
}

/// Signs with one key.
pub fn sign(key: &RsaPrivateKey, data: &[u8]) -> String {
    let signer = SigningKey::<Sha256>::new(key.clone());
    STANDARD.encode(signer.sign_with_rng(&mut OsRng, data).to_bytes())
}

/// Checks one signature.
pub fn verify(key: &RsaPublicKey, data: &[u8], signature_b64: &str) -> bool {
    let Ok(sig) = STANDARD.decode(signature_b64.trim()) else { return false };
    let Ok(sig) = Signature::try_from(sig.as_slice()) else { return false };
    VerifyingKey::<Sha256>::new(key.clone()).verify(data, &sig).is_ok()
}

/// What a WOPI host does with the three headers: accepts when the proof verifies with the
/// current key, or the old proof with the current key, or the proof with the old key; and the
/// timestamp is at most 20 minutes old.
pub fn host_accepts(
    current: &RsaPublicKey,
    old: Option<&RsaPublicKey>,
    access_token: &str,
    url: &str,
    proof: &str,
    proof_old: Option<&str>,
    ticks: u64,
    now: u64,
) -> bool {
    if now.saturating_sub(ticks) > MAX_AGE_TICKS {
        return false;
    }
    let data = signed_data(access_token, url, ticks);
    verify(current, &data, proof) || proof_old.is_some_and(|p| verify(current, &data, p)) || old.is_some_and(|o| verify(o, &data, proof))
}

/// craft-live's key pair and the previous one (kept for one rotation, so hosts that cached the
/// old discovery still accept the requests).
#[derive(Clone, Debug)]
pub struct ProofKeys {
    pub current: RsaPrivateKey,
    pub old: Option<RsaPrivateKey>,
}

/// The headers of a signed request.
#[derive(Clone, Debug)]
pub struct ProofHeaders {
    pub proof: String,
    pub proof_old: String,
    pub timestamp: String,
}

impl ProofKeys {
    pub fn generate() -> Result<ProofKeys, String> {
        Ok(ProofKeys { current: RsaPrivateKey::new(&mut OsRng, 2048).map_err(|e| e.to_string())?, old: None })
    }

    /// Loads `proof-key.pem` (and `proof-key-old.pem`) from the folder, making a key the first
    /// time.
    pub fn load_or_create(dir: &Path) -> Result<ProofKeys, String> {
        let cur = dir.join("proof-key.pem");
        let old = dir.join("proof-key-old.pem");
        let read = |p: &Path| -> Result<Option<RsaPrivateKey>, String> {
            match std::fs::read_to_string(p) {
                Ok(text) => RsaPrivateKey::from_pkcs8_pem(&text).map(Some).map_err(|e| format!("{}: {e}", p.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{}: {e}", p.display())),
            }
        };
        let old_key = read(&old)?;
        match read(&cur)? {
            Some(current) => Ok(ProofKeys { current, old: old_key }),
            None => {
                let keys = ProofKeys::generate()?;
                write_key(&cur, &keys.current)?;
                Ok(keys)
            }
        }
    }

    /// Makes a new key and keeps the current one as the old one.
    pub fn rotate(dir: &Path) -> Result<ProofKeys, String> {
        let keys = ProofKeys::load_or_create(dir)?;
        let fresh = ProofKeys::generate()?;
        write_key(&dir.join("proof-key-old.pem"), &keys.current)?;
        write_key(&dir.join("proof-key.pem"), &fresh.current)?;
        Ok(ProofKeys { current: fresh.current, old: Some(keys.current) })
    }

    /// The discovery's `(current, old)`; the old one is the current one until a rotation.
    pub fn discovery(&self) -> (PublicParts, PublicParts) {
        let cur = public_parts(&self.current.to_public_key());
        let old = self.old.as_ref().map(|k| public_parts(&k.to_public_key())).unwrap_or_else(|| cur.clone());
        (cur, old)
    }

    /// The headers for a request to `url` (the full URL, with `access_token`).
    pub fn headers(&self, access_token: &str, url: &str) -> ProofHeaders {
        let ticks = now_ticks();
        let data = signed_data(access_token, url, ticks);
        let proof = sign(&self.current, &data);
        let proof_old = match &self.old {
            Some(o) => sign(o, &data),
            None => proof.clone(),
        };
        ProofHeaders { proof, proof_old, timestamp: ticks.to_string() }
    }
}

fn write_key(path: &Path, key: &RsaPrivateKey) -> Result<(), String> {
    let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(|e| e.to_string())?;
    crate::store::write_private(path, pem.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_like_a_host() {
        let keys = ProofKeys::generate().unwrap();
        let old = ProofKeys::generate().unwrap();
        let rotated = ProofKeys { current: keys.current.clone(), old: Some(old.current.clone()) };
        let url = "https://cloud.example/index.php/apps/richdocuments/wopi/files/12_oc?access_token=tok";
        let h = rotated.headers("tok", url);
        let ticks: u64 = h.timestamp.parse().unwrap();
        let (cur, oldp) = rotated.discovery();
        let cur_key = from_modulus_exponent(&cur.modulus, &cur.exponent).unwrap();
        let old_key = from_csp_blob(&STANDARD.decode(&oldp.value).unwrap()).unwrap();
        assert_eq!(old_key, old.current.to_public_key());
        assert!(host_accepts(&cur_key, Some(&old_key), "tok", url, &h.proof, Some(&h.proof_old), ticks, now_ticks()));
        // A host that still has the old key as current accepts X-WOPI-ProofOld.
        assert!(host_accepts(&old_key, None, "tok", url, "AAAA", Some(&h.proof_old), ticks, now_ticks()));
        // Another token, another URL, an old timestamp: refused.
        assert!(!host_accepts(&cur_key, Some(&old_key), "tok2", url, &h.proof, Some(&h.proof_old), ticks, now_ticks()));
        assert!(!host_accepts(&cur_key, None, "tok", "https://evil/", &h.proof, None, ticks, now_ticks()));
        assert!(!host_accepts(&cur_key, None, "tok", url, &h.proof, None, ticks, ticks + MAX_AGE_TICKS + 1));
        // Upper-casing: the host compares the URL in upper case.
        assert!(host_accepts(&cur_key, None, "tok", &url.to_uppercase(), &h.proof, None, ticks, now_ticks()));
    }

    #[test]
    fn csp_blob_layout() {
        let keys = ProofKeys::generate().unwrap();
        let pubk = keys.current.to_public_key();
        let blob = csp_blob(&pubk);
        assert_eq!(blob.len(), 20 + 256);
        assert_eq!(&blob[..8], &[6, 2, 0, 0, 0, 0xa4, 0, 0]);
        assert_eq!(&blob[8..12], b"RSA1");
        assert_eq!(u32::from_le_bytes(blob[12..16].try_into().unwrap()), 2048);
        assert_eq!(u32::from_le_bytes(blob[16..20].try_into().unwrap()), 65537);
        assert_eq!(from_csp_blob(&blob), Some(pubk));
        assert_eq!(from_csp_blob(&blob[..30]), None);
    }

    #[test]
    fn ticks_are_dotnet_ticks() {
        // 2026-01-01T00:00:00Z is 639_028_224_000_000_000 ticks.
        let t = now_ticks();
        assert!(t > 639_028_224_000_000_000);
        assert_eq!(&signed_data("ab", "x", 1)[..], &[0, 0, 0, 2, b'a', b'b', 0, 0, 0, 1, b'X', 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1]);
    }
}
