//! Bedrock's login crypto: the ES384 identity key that signs the login chain,
//! the X25519 ephemeral key whose public half travels in the client-data JWT,
//! and the AES-256-GCM stream cipher `ServerToClientHandshake` switches on.
//!
//! The client we analysed in `libminecraftpe.so` links mbedTLS (DTLS) and WebRTC
//! for NetherNet; its login JWT chain uses ECDSA over P-384 (`ES384`) and the
//! handshake uses X25519. `ring` supplies exactly those primitives.

use crate::error::{Error, Result};
use base64::Engine;
use ring::{
    aead, agreement, digest, rand,
    signature::{EcdsaKeyPair, KeyPair, ECDSA_P384_SHA384_FIXED_SIGNING},
};

pub use base64::engine::general_purpose::STANDARD as B64;
pub use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;

/// The Secp384r1 SPKI prefix, so a raw P-384 point can be wrapped as DER.
const P384_SPKI_PREFIX: [u8; 23] = [
    0x30, 0x76, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b,
    0x81, 0x04, 0x00, 0x22, 0x03, 0x62, 0x00,
];
/// The X25519 SPKI prefix (RFC 8410).
const X25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e, 0x03, 0x21, 0x00,
];

/// The session's ES384 identity key: signs the login chain and the client-data
/// JWT. The matching public key is what the server trusts for the chain.
pub struct IdentityKey {
    key_pair: EcdsaKeyPair,
    pkcs8: Vec<u8>,
    public_p384: Vec<u8>,
}

impl IdentityKey {
    pub fn generate() -> Result<Self> {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(
            &ECDSA_P384_SHA384_FIXED_SIGNING,
            &rand::SystemRandom::new(),
        )
        .map_err(|_| Error::auth("could not generate an ES384 identity key"))?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let key_pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P384_SHA384_FIXED_SIGNING,
            pkcs8,
            &rand::SystemRandom::new(),
        )
        .map_err(|_| Error::auth("bad ES384 pkcs8"))?;
        let point = key_pair.public_key().as_ref().to_vec();
        Ok(IdentityKey {
            key_pair,
            pkcs8: pkcs8.to_vec(),
            public_p384: point,
        })
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// An independent copy (ring's key type is not `Clone`).
    pub fn try_clone(&self) -> Result<Self> {
        Self::from_pkcs8(&self.pkcs8)
    }

    /// The uncompressed EC point `0x04 || X || Y` (97 bytes for P-384).
    pub fn public_point(&self) -> &[u8] {
        &self.public_p384
    }

    /// The public key as DER SubjectPublicKeyInfo (the `x5u` claim).
    pub fn public_der(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(P384_SPKI_PREFIX.len() + self.public_p384.len());
        out.extend_from_slice(&P384_SPKI_PREFIX);
        out.extend_from_slice(&self.public_p384);
        out
    }

    /// Sign bytes with ES384, returning the JOSE `r||s` signature (96 bytes).
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        let sig = self
            .key_pair
            .sign(&rand::SystemRandom::new(), msg)
            .map_err(|_| Error::auth("ES384 signing failed"))?;
        Ok(sig.as_ref().to_vec())
    }

    /// Build a compact JWS: `base64url(header).base64url(payload).base64url(sig)`.
    pub fn sign_jwt(&self, header_json: &str, payload_json: &str) -> Result<String> {
        let signing_input = format!(
            "{}.{}",
            B64URL.encode(header_json),
            B64URL.encode(payload_json)
        );
        let sig = self.sign(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", B64URL.encode(sig)))
    }
}

/// The session's X25519 key: its public half is embedded in the client-data JWT
/// (`x5u`) so the server can derive the shared secret for `ServerToClientHandshake`.
pub struct HandshakeKey {
    private: agreement::EphemeralPrivateKey,
    public_raw: [u8; 32],
}

impl HandshakeKey {
    pub fn generate() -> Result<Self> {
        let private = agreement::EphemeralPrivateKey::generate(
            &agreement::X25519,
            &rand::SystemRandom::new(),
        )
        .map_err(|_| Error::auth("could not generate an X25519 key"))?;
        let public = private
            .compute_public_key()
            .map_err(|_| Error::auth("could not compute the X25519 public key"))?;
        let mut public_raw = [0u8; 32];
        public_raw.copy_from_slice(public.as_ref());
        Ok(HandshakeKey {
            private,
            public_raw,
        })
    }

    pub fn public_raw(&self) -> [u8; 32] {
        self.public_raw
    }

    pub fn public_der(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(X25519_SPKI_PREFIX.len() + 32);
        out.extend_from_slice(&X25519_SPKI_PREFIX);
        out.extend_from_slice(&self.public_raw);
        out
    }

    /// Derive the 32-byte shared secret from the server's X25519 public key.
    /// Consumes the private key (the ephemeral key is single-use).
    pub fn agree(self, server_public: &[u8]) -> Result<[u8; 32]> {
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, server_public);
        agreement::agree_ephemeral(self.private, &peer, |secret| {
            let mut key = [0u8; 32];
            key.copy_from_slice(secret);
            key
        })
        .map_err(|_| Error::auth("X25519 key agreement failed"))
    }
}

/// Pull the raw X25519 public key out of a DER SPKI blob (`x5u`).
pub fn parse_spki_x25519(der: &[u8]) -> Result<Vec<u8>> {
    if der.len() >= 32 {
        Ok(der[der.len() - 32..].to_vec())
    } else {
        Err(Error::auth("server key is not an X25519 SPKI"))
    }
}

/// Derive the AES-256-GCM key and IV from the shared secret and the salt the
/// server put in the handshake JWT: `key = SHA-256(shared || salt)`,
/// `iv = SHA-256(salt || shared)[..16]`.
pub fn derive_key_iv(shared: &[u8; 32], salt: &[u8]) -> ([u8; 32], [u8; 16]) {
    let key = sha256_concat(shared, salt);
    let iv_full = sha256_concat(salt, shared);
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&iv_full[..16]);
    (key, iv)
}

fn sha256_concat(a: &[u8], b: &[u8]) -> [u8; 32] {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(a);
    ctx.update(b);
    let d = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// The post-handshake stream cipher: AES-256-GCM with a per-direction counter.
///
/// The key derivation above is stable. The nonce layout (the 16-byte IV mixed
/// with a packet counter) is the one part that can only be confirmed against a
/// live encrypted server, so it is isolated here.
pub struct Cipher {
    key: aead::LessSafeKey,
    iv: [u8; 16],
    counter: u64,
}

impl Cipher {
    pub fn new(key: [u8; 32], iv: [u8; 16]) -> Self {
        let unbound =
            aead::UnboundKey::new(&aead::AES_256_GCM, &key).expect("valid AES key length");
        Cipher {
            key: aead::LessSafeKey::new(unbound),
            iv,
            counter: 0,
        }
    }

    fn nonce(&mut self) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.iv[..4]);
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter = self.counter.wrapping_add(1);
        nonce
    }

    /// Encrypt in place, appending the GCM tag.
    pub fn encrypt(&mut self, data: &mut Vec<u8>) -> Result<()> {
        let nonce = self.nonce();
        self.key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                data,
            )
            .map_err(|_| Error::other("AES-GCM seal failed"))
    }

    /// Decrypt in place, stripping the GCM tag.
    pub fn decrypt(&mut self, data: &mut Vec<u8>) -> Result<()> {
        let nonce = self.nonce();
        let plain = self
            .key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::empty(),
                data,
            )
            .map_err(|_| Error::other("AES-GCM open failed"))?;
        let len = plain.len();
        data.truncate(len);
        Ok(())
    }
}

/// Parse the `ServerToClientHandshake` JWT and pull out the server key (`x5u`)
/// and `salt`.
pub fn parse_handshake_jwt(jwt: &str) -> Result<(String, Vec<u8>)> {
    let mut parts = jwt.split('.');
    let header = parts
        .next()
        .and_then(|p| B64URL.decode(p).ok())
        .ok_or_else(|| Error::auth("handshake JWT has no header"))?;
    let payload = parts
        .next()
        .and_then(|p| B64URL.decode(p).ok())
        .ok_or_else(|| Error::auth("handshake JWT has no payload"))?;
    let header: serde_json::Value =
        serde_json::from_slice(&header).map_err(|_| Error::auth("handshake header is not JSON"))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| Error::auth("handshake payload is not JSON"))?;
    let x5u = header
        .get("x5u")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::auth("handshake header has no x5u"))?
        .to_owned();
    let salt = payload
        .get("salt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::auth("handshake payload has no salt"))?;
    let salt = B64URL
        .decode(salt)
        .or_else(|_| B64.decode(salt))
        .map_err(|_| Error::auth("handshake salt is not base64"))?;
    Ok((x5u, salt))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn es384_jwt_verifies_shape() {
        let key = IdentityKey::generate().unwrap();
        let jwt = key
            .sign_jwt(r#"{"alg":"ES384","typ":"JWT"}"#, r#"{"test":1}"#)
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        // P-384 JOSE signatures are 96 bytes.
        assert_eq!(B64URL.decode(parts[2]).unwrap().len(), 96);
        assert!(key.public_der().len() > 90);
    }

    #[test]
    fn x25519_agreement_is_symmetric() {
        let a = HandshakeKey::generate().unwrap();
        let b = HandshakeKey::generate().unwrap();
        let (a_pub, b_pub) = (a.public_raw(), b.public_raw());
        let ab = a.agree(&b_pub).unwrap();
        let ba = b.agree(&a_pub).unwrap();
        assert_eq!(ab, ba);
        assert_eq!(a_pub.len(), 32);
    }

    #[test]
    fn key_iv_derivation_is_order_sensitive() {
        let shared = [7u8; 32];
        let (key, iv) = derive_key_iv(&shared, b"salt");
        let (key2, _) = derive_key_iv(&shared, b"tlas");
        assert_ne!(key, key2);
        assert_eq!(iv.len(), 16);
    }

    #[test]
    fn cipher_round_trips_and_advances() {
        let mut enc = Cipher::new([1u8; 32], [2u8; 16]);
        let mut dec = Cipher::new([1u8; 32], [2u8; 16]);
        for msg in [b"hello".to_vec(), b"world!".to_vec()] {
            let mut a = msg.clone();
            enc.encrypt(&mut a).unwrap();
            assert_ne!(a, msg);
            dec.decrypt(&mut a).unwrap();
            assert_eq!(a, msg);
        }
    }

    #[test]
    fn parses_a_handshake_jwt() {
        let header = B64URL.encode(r#"{"alg":"ES384","x5u":"AAAA"}"#);
        let salt = B64URL.encode([9u8; 8]);
        let payload = B64URL.encode(format!(r#"{{"salt":"{salt}"}}"#));
        let jwt = format!("{header}.{payload}.");
        let (x5u, s) = parse_handshake_jwt(&jwt).unwrap();
        assert_eq!(x5u, "AAAA");
        assert_eq!(s, vec![9u8; 8]);
    }
}
