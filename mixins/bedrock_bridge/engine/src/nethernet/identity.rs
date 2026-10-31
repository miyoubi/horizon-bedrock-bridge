//! NetherNet identity assertions: the ES384 JWT (with a `cpk` P-384 key) and the
//! detached JWS over the SDP fingerprints, carried in an `a=identity:` line.
//!
//! Ported from the `nethernet` reference's `identity` module, implemented with
//! `ring` (P-384 fixed ES384) rather than `p384`. A client can self-sign its
//! token: the default server trust accepts any well-formed unexpired token and
//! binds it to the DTLS certificate through the fingerprint signature.

use crate::crypto::{IdentityKey, B64, B64URL};
use crate::error::{Error, Result};
use base64::Engine;
use ring::signature::{UnparsedPublicKey, ECDSA_P384_SHA384_FIXED};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Header of a compact JWS.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub alg: String,
    #[serde(default)]
    pub kid: Option<String>,
}

/// A compact JWS (`header.payload.signature`).
#[derive(Clone, Debug)]
pub struct Jws {
    pub header: Header,
    pub signing_input: String,
    pub payload: Vec<u8>,
    pub signature: Vec<u8>,
}

impl Jws {
    pub fn parse(compact: &str) -> Result<Self> {
        let mut parts = compact.split('.');
        let (Some(header), Some(payload), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::decode("JWS: expected three parts"));
        };
        Ok(Self {
            header: serde_json::from_slice(&decode(header)?)
                .map_err(|e| Error::decode(format!("JWS header: {e}")))?,
            signing_input: format!("{header}.{payload}"),
            payload: decode(payload)?,
            signature: decode(signature)?,
        })
    }

    /// Parse a detached `header..signature`, taking the payload from `payload`.
    pub fn parse_detached(compact: &str, payload: &str) -> Result<Self> {
        let mut parts = compact.split('.');
        let (Some(header), Some(""), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::decode("JWS: expected a detached serialization"));
        };
        Ok(Self {
            header: serde_json::from_slice(&decode(header)?)
                .map_err(|e| Error::decode(format!("JWS header: {e}")))?,
            signing_input: format!("{header}.{}", B64URL.encode(payload)),
            payload: payload.as_bytes().to_vec(),
            signature: decode(signature)?,
        })
    }

    /// Sign `payload` as a compact ES384 JWS (ring returns the JOSE `r||s` form).
    pub fn sign(key: &IdentityKey, payload: &str) -> Result<String> {
        let (header, payload, signature) = Self::sign_parts(key, payload)?;
        Ok(format!("{header}.{payload}.{signature}"))
    }

    /// Sign `payload` as a detached `header..signature` ES384 JWS.
    pub fn sign_detached(key: &IdentityKey, payload: &str) -> Result<String> {
        let (header, _, signature) = Self::sign_parts(key, payload)?;
        Ok(format!("{header}..{signature}"))
    }

    fn sign_parts(key: &IdentityKey, payload: &str) -> Result<(String, String, String)> {
        // Bedrock JWTs carry the signing key in the `x5u` header; servers use it
        // to tell a self-signed token from a service-issued one.
        let header = B64URL.encode(format!(
            r#"{{"alg":"ES384","x5u":"{}"}}"#,
            crate::crypto::B64.encode(key.public_der())
        ));
        let encoded = B64URL.encode(payload);
        let signature = key.sign(format!("{header}.{encoded}").as_bytes())?;
        Ok((header, encoded, B64URL.encode(signature)))
    }

    /// Verify an ES384 signature against a P-384 point (`0x04 || X || Y`).
    pub fn verify_es384(&self, point: &[u8]) -> Result<()> {
        if self.header.alg != "ES384" {
            return Err(Error::decode(format!(
                "expected ES384, got {}",
                self.header.alg
            )));
        }
        UnparsedPublicKey::new(&ECDSA_P384_SHA384_FIXED, point)
            .verify(self.signing_input.as_bytes(), &self.signature)
            .map_err(|_| Error::auth("fingerprint signature mismatch"))
    }

    pub fn claims(&self) -> Result<Claims> {
        let values: Map<String, Value> = serde_json::from_slice(&self.payload)
            .map_err(|e| Error::decode(format!("claims: {e}")))?;
        Ok(Claims { values })
    }
}

/// Token claims.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    values: Map<String, Value>,
}

impl Claims {
    pub fn values(&self) -> &Map<String, Value> {
        &self.values
    }
    pub fn string(&self, claim: &str) -> Option<&str> {
        self.values.get(claim).and_then(Value::as_str)
    }
    pub fn issuer(&self) -> Option<&str> {
        self.string("iss")
    }
    pub fn xuid(&self) -> Option<&str> {
        self.string("xid")
    }
    pub fn display_name(&self) -> Option<&str> {
        self.string("xname")
    }
    pub fn expiry(&self) -> Option<SystemTime> {
        let s = self.values.get("exp").and_then(Value::as_u64)?;
        Some(UNIX_EPOCH + Duration::from_secs(s))
    }

    /// Refuse a token with no `exp`, or expired by more than 60 seconds.
    pub fn check_expiry(&self, now: SystemTime) -> Result<()> {
        let expiry = self
            .expiry()
            .ok_or_else(|| Error::auth("the token carries no expiry"))?;
        if now <= expiry + Duration::from_secs(60) {
            Ok(())
        } else {
            Err(Error::auth("the token has expired"))
        }
    }

    /// The P-384 point from `cpk`, as a JWK object or a base64 SPKI string.
    pub fn client_public_key(&self) -> Result<Vec<u8>> {
        match self.values.get("cpk") {
            None => Err(Error::auth("the token carries no cpk")),
            Some(Value::String(b64)) => {
                let der = B64
                    .decode(b64)
                    .map_err(|_| Error::auth("cpk is not base64"))?;
                // An SPKI DER for P-384 ends with the uncompressed point.
                let start = der.len().saturating_sub(97);
                if der.len() >= 97 && der[start] == 0x04 {
                    Ok(der[start..].to_vec())
                } else {
                    Err(Error::auth("cpk is not a P-384 SPKI"))
                }
            }
            Some(Value::Object(obj)) => {
                if obj.get("kty").and_then(Value::as_str) != Some("EC")
                    || obj.get("crv").and_then(Value::as_str) != Some("P-384")
                {
                    return Err(Error::auth("cpk is not an EC P-384 key"));
                }
                let x = obj
                    .get("x")
                    .and_then(Value::as_str)
                    .and_then(|v| B64URL.decode(v).ok())
                    .filter(|b| b.len() == 48);
                let y = obj
                    .get("y")
                    .and_then(Value::as_str)
                    .and_then(|v| B64URL.decode(v).ok())
                    .filter(|b| b.len() == 48);
                let (Some(x), Some(y)) = (x, y) else {
                    return Err(Error::auth("cpk coordinates are invalid"));
                };
                let mut point = Vec::with_capacity(97);
                point.push(0x04);
                point.extend_from_slice(&x);
                point.extend_from_slice(&y);
                Ok(point)
            }
            Some(_) => Err(Error::auth("cpk is neither a string nor a JWK")),
        }
    }
}

/// Identity provider block of an assertion envelope.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Idp {
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub protocol: String,
}

/// Token plus a detached ES384 signature over the SDP fingerprints.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Assertion {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub fingerprints: String,
}

/// The decoded `a=identity` value.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub idp: Idp,
    pub assertion: Assertion,
}

#[derive(Serialize, Deserialize)]
struct Raw {
    #[serde(default)]
    idp: Idp,
    #[serde(default)]
    assertion: String,
}

impl Identity {
    pub fn from_json(json: &str) -> Result<Self> {
        let raw: Raw =
            serde_json::from_str(json).map_err(|e| Error::decode(format!("identity: {e}")))?;
        let assertion = serde_json::from_str(&raw.assertion)
            .map_err(|e| Error::decode(format!("assertion: {e}")))?;
        Ok(Self {
            idp: raw.idp,
            assertion,
        })
    }

    pub fn from_base64(value: &str) -> Result<Self> {
        let bytes = B64
            .decode(value.trim())
            .map_err(|e| Error::decode(format!("identity base64: {e}")))?;
        let json =
            String::from_utf8(bytes).map_err(|e| Error::decode(format!("identity utf8: {e}")))?;
        Self::from_json(&json)
    }

    pub fn from_sdp(sdp: &str) -> Result<Self> {
        let value = sdp
            .split(['\r', '\n'])
            .find_map(|line| line.strip_prefix("a=identity:"))
            .ok_or_else(|| Error::auth("the description carries no identity"))?;
        Self::from_base64(value)
    }

    pub fn to_json(&self) -> Result<String> {
        let raw = Raw {
            idp: self.idp.clone(),
            assertion: serde_json::to_string(&self.assertion)
                .map_err(|e| Error::decode(format!("assertion: {e}")))?,
        };
        serde_json::to_string(&raw).map_err(|e| Error::decode(format!("identity: {e}")))
    }

    pub fn to_base64(&self) -> Result<String> {
        Ok(B64.encode(self.to_json()?))
    }
}

/// The `(algorithm, digest)` pairs of the `a=fingerprint` lines, in order.
pub fn sdp_fingerprints(sdp: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in sdp
        .split(['\r', '\n'])
        .filter_map(|line| line.strip_prefix("a=fingerprint:"))
    {
        let Some((alg, digest)) = line.trim().split_once(' ') else {
            return Err(Error::decode(format!("invalid fingerprint line: {line}")));
        };
        out.push((alg.to_string(), digest.to_string()));
    }
    Ok(out)
}

/// The signed payload for a list of `(algorithm, digest)` pairs.
pub fn fingerprint_payload(fingerprints: &[(String, String)]) -> String {
    let entries: Vec<_> = fingerprints
        .iter()
        .map(|(algorithm, digest)| json!({"algorithm": algorithm, "digest": digest}))
        .collect();
    json!({"fingerprint": entries}).to_string()
}

/// The canonical JSON the fingerprint signature covers.
pub fn canonical_fingerprint_json(sdp: &str) -> Result<String> {
    Ok(fingerprint_payload(&sdp_fingerprints(sdp)?))
}

const EMPTY_FINGERPRINTS: &str = "{\"fingerprint\":[]}";
/// Default token lifetime.
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
/// Domain used when none is given (peers refuse an empty one).
pub const DEFAULT_DOMAIN: &str = "self";

/// A client's identity: a P-384 key that signs a self-issued token and the SDP
/// fingerprints.
pub struct ClientIdentity {
    key: IdentityKey,
    domain: String,
    lifetime: Option<Duration>,
    /// A token minted by the Minecraft authorization service (RS256, `cpk` =
    /// our key). When set it replaces the self-signed token.
    external_token: Option<String>,
}

impl ClientIdentity {
    pub fn new(key: IdentityKey) -> Self {
        Self {
            key,
            domain: DEFAULT_DOMAIN.to_string(),
            lifetime: Some(DEFAULT_TOKEN_LIFETIME),
            external_token: None,
        }
    }

    /// Use a token minted by the Minecraft authorization service instead of the
    /// self-signed one. `key` must be the same P-384 key whose public DER was
    /// sent as the token's `cpk`, so the fingerprint signature still validates.
    pub fn external(key: IdentityKey, token: impl Into<String>) -> Self {
        Self {
            key,
            domain: DEFAULT_DOMAIN.to_string(),
            lifetime: None,
            external_token: Some(token.into()),
        }
    }

    /// The P-384 public key as DER SubjectPublicKeyInfo (the `publicKey` sent to
    /// `multiplayer/session/start`).
    pub fn public_key_der(&self) -> Vec<u8> {
        self.key.public_der()
    }

    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        let d = domain.into();
        self.domain = if d.is_empty() {
            DEFAULT_DOMAIN.into()
        } else {
            d
        };
        self
    }

    pub fn with_lifetime(mut self, lifetime: Option<Duration>) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// The token's `cpk`: the public key as a JWK object.
    fn cpk(&self) -> Value {
        let point = self.key.public_point();
        // Uncompressed point: 0x04 || X(48) || Y(48).
        let x = &point[1..49];
        let y = &point[49..97];
        json!({
            "kty": "EC",
            "crv": "P-384",
            "x": B64URL.encode(x),
            "y": B64URL.encode(y),
        })
    }

    /// The identity token: the externally minted one, or a self-signed token.
    pub fn token(&self, now: SystemTime) -> Result<String> {
        if let Some(token) = &self.external_token {
            return Ok(token.clone());
        }
        let mut claims = json!({ "cpk": self.cpk(), "iat": seconds(now) });
        if !self.domain.is_empty() {
            claims["iss"] = json!(self.domain);
        }
        if let Some(lifetime) = self.lifetime {
            claims["exp"] = json!(seconds(now + lifetime));
        }
        Jws::sign(&self.key, &claims.to_string())
    }

    /// The base64 identity value signing the fingerprints of `sdp`.
    pub fn identity_value(&self, sdp: &str, now: SystemTime) -> Result<String> {
        let payload = canonical_fingerprint_json(sdp)?;
        Identity {
            idp: Idp {
                domain: self.domain.clone(),
                protocol: "default".to_string(),
            },
            assertion: Assertion {
                token: self.token(now)?,
                fingerprints: Jws::sign_detached(&self.key, &payload)?,
            },
        }
        .to_base64()
    }

    /// Insert a signed `a=identity:` line above the first media section.
    pub fn augment(&self, sdp: &str, now: SystemTime) -> Result<String> {
        let attribute = format!("a=identity:{}", self.identity_value(sdp, now)?);
        let eol = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
        let mut out = String::with_capacity(sdp.len() + attribute.len() + eol.len());
        let mut inserted = false;
        for line in sdp.split_inclusive('\n') {
            if !inserted && line.starts_with("m=") {
                out.push_str(&attribute);
                out.push_str(eol);
                inserted = true;
            }
            out.push_str(line);
        }
        if !inserted {
            out.push_str(&attribute);
            out.push_str(eol);
        }
        Ok(out)
    }
}

/// Verify an incoming description's self-issued assertion: expiry, then that the
/// `cpk` signed its SDP fingerprints. Returns the claims on success.
pub fn validate_sdp(sdp: &str, now: SystemTime) -> Result<Claims> {
    let identity = Identity::from_sdp(sdp)?;
    let jws = Jws::parse(&identity.assertion.token)?;
    let claims = jws.claims()?;
    claims.check_expiry(now)?;

    let fingerprints = canonical_fingerprint_json(sdp)?;
    if fingerprints == EMPTY_FINGERPRINTS {
        return Err(Error::auth("the description has no fingerprints"));
    }
    let point = claims.client_public_key()?;
    let detached = Jws::parse_detached(&identity.assertion.fingerprints, &fingerprints)?;
    detached.verify_es384(&point)?;
    Ok(claims)
}

fn decode(value: &str) -> Result<Vec<u8>> {
    B64URL
        .decode(value)
        .map_err(|e| Error::decode(format!("base64url: {e}")))
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUIDE_SDP: &str = "v=0\r\na=fingerprint:sha-256 4A:AD:B9:B1:3F:82:18:3B:54:02:12:DF:3E:5D:49:6B:19:E5:7C:AB\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
    const GUIDE_JSON: &str = "{\"fingerprint\":[{\"algorithm\":\"sha-256\",\"digest\":\"4A:AD:B9:B1:3F:82:18:3B:54:02:12:DF:3E:5D:49:6B:19:E5:7C:AB\"}]}";

    const VANILLA_ENVELOPE: &str = "eyJpZHAiOnsiZG9tYWluIjoiYXV0aD4+Pj8uZXhhbXBsZSIsInByb3RvY29sIjoiZGVmYXVsdCJ9LCJhc3NlcnRpb24iOiJ7XCJ0b2tlblwiOlwiYWEuYmIuY2NcIixcImZpbmdlcnByaW50c1wiOlwiaGguLnNzXCJ9In0=";

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn the_guide_fingerprint_canonicalizes_to_the_documented_json() {
        assert_eq!(canonical_fingerprint_json(GUIDE_SDP).unwrap(), GUIDE_JSON);
    }

    #[test]
    fn the_vanilla_envelope_decodes() {
        let id = Identity::from_base64(VANILLA_ENVELOPE).unwrap();
        assert_eq!(id.idp.domain, "auth>>>?.example");
        assert_eq!(id.idp.protocol, "default");
        assert_eq!(id.assertion.token, "aa.bb.cc");
        assert_eq!(id.assertion.fingerprints, "hh..ss");
        assert_eq!(id.to_base64().unwrap(), VANILLA_ENVELOPE);
    }

    #[test]
    fn a_self_signed_identity_round_trips_and_validates() {
        let key = IdentityKey::generate().unwrap();
        let identity = ClientIdentity::new(key).with_domain("example.test");
        let augmented = identity.augment(GUIDE_SDP, now()).unwrap();

        assert!(augmented.contains("a=identity:"));
        let claims = validate_sdp(&augmented, now()).unwrap();
        assert_eq!(claims.issuer(), Some("example.test"));
        assert_eq!(
            claims.client_public_key().unwrap(),
            identity.key.public_point()
        );
    }

    #[test]
    fn a_tampered_fingerprint_is_rejected() {
        let identity = ClientIdentity::new(IdentityKey::generate().unwrap());
        let augmented = identity.augment(GUIDE_SDP, now()).unwrap();
        let tampered = augmented.replace("AB\r\n", "EF\r\n");
        assert!(validate_sdp(&tampered, now()).is_err());
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let identity = ClientIdentity::new(IdentityKey::generate().unwrap())
            .with_lifetime(Some(Duration::from_secs(10)));
        let augmented = identity.augment(GUIDE_SDP, now()).unwrap();
        assert!(validate_sdp(&augmented, now() + Duration::from_secs(600)).is_err());
    }

    #[test]
    fn the_identity_is_inserted_above_the_media_section() {
        let identity = ClientIdentity::new(IdentityKey::generate().unwrap());
        let augmented = identity.augment(GUIDE_SDP, now()).unwrap();
        let lines: Vec<&str> = augmented.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("a=identity:"))
            .unwrap();
        assert!(lines[at + 1].starts_with("m="));
    }
}
