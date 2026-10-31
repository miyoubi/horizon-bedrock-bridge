//! Xbox Live sign-in and the Bedrock login JWT chain.
//!
//! The flow (mirrored from the working TunnelMC/gophertunnel reference and the
//! native `XboxLiveIdentity`/`AuthServiceApi` classes in `libminecraftpe.so`):
//!
//! 1. A Microsoft OAuth token comes from the **device-code** flow — no token has
//!    to be pasted by hand. It is cached (refresh token) under the mod's data
//!    directory.
//! 2. Xbox Live: user, device and title tokens (each with an ES256
//!    proof-of-possession key and the `x-xbl-contract-version` signature
//!    header), then an XSTS token for `https://multiplayer.minecraft.net/`.
//! 3. `multiplayer.minecraft.net/authentication` returns the three inherited
//!    chain JWTs. The client prepends its **own** self-signed JWT, signed with
//!    its ES384 identity key, whose `identityPublicKey` points at the inherited
//!    root so the server can validate the whole chain.
//! 4. A second JWT carries the skin data, signed with the same identity key.

use crate::crypto::{HandshakeKey, IdentityKey, B64, B64URL};
use crate::error::{Error, Result};
use base64::Engine;
use ring::{
    rand,
    signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The Bedrock (Android) title client id, used with the LIVE device-code flow.
pub const CLIENT_ID: &str = "0000000048183522";
/// The scope that yields an Xbox Live capable token.
pub const SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";

const DEVICE_CODE_URL: &str = "https://login.live.com/oauth20_connect.srf";
const TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
const USER_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const DEVICE_AUTH_URL: &str = "https://device.auth.xboxlive.com/device/authenticate";
#[allow(dead_code)]
const TITLE_AUTH_URL: &str = "https://title.auth.xboxlive.com/title/authenticate";
const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MINECRAFT_AUTH_URL: &str = "https://multiplayer.minecraft.net/authentication";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
}

/// A Microsoft account token from the device-code flow.
#[derive(Clone, Debug)]
pub struct MsaToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// The device code the player must enter, plus the polling interval.
#[derive(Clone, Debug)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub device_code: String,
    pub interval: u64,
}

/// Start the device-code flow. The caller shows `user_code` and
/// `verification_uri`, then calls [`poll_device_code`].
pub fn begin_device_code() -> Result<DeviceCode> {
    let response = agent()
        .post(DEVICE_CODE_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("response_type", "device_code"),
        ])
        .map_err(|e| Error::auth(format!("device code request failed: {e}")))?;
    let json: serde_json::Value = serde_json::from_str(&response.into_string().unwrap_or_default())
        .map_err(|e| Error::auth(format!("device code response was not JSON: {e}")))?;
    Ok(DeviceCode {
        user_code: json["user_code"].as_str().unwrap_or_default().to_owned(),
        verification_uri: json["verification_uri"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        device_code: json["device_code"].as_str().unwrap_or_default().to_owned(),
        interval: json["interval"].as_u64().unwrap_or(5).max(1),
    })
}

/// Poll the token endpoint once. `Ok(None)` means the player has not finished
/// signing in yet.
pub fn poll_device_code(code: &DeviceCode) -> Result<Option<MsaToken>> {
    match agent()
        .post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "device_code"),
            ("device_code", &code.device_code),
        ]) {
        Ok(response) => {
            let json: serde_json::Value =
                serde_json::from_str(&response.into_string().unwrap_or_default())
                    .map_err(|e| Error::auth(format!("token response was not JSON: {e}")))?;
            let access = json["access_token"]
                .as_str()
                .ok_or_else(|| Error::auth("no access_token"))?;
            Ok(Some(MsaToken {
                access_token: access.to_owned(),
                refresh_token: json["refresh_token"].as_str().map(str::to_owned),
            }))
        }
        Err(ureq::Error::Status(_, response)) => {
            let text = response.into_string().unwrap_or_default();
            let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            match json["error"].as_str().unwrap_or("") {
                "authorization_pending" | "slow_down" => Ok(None),
                other => Err(Error::auth(format!("device sign-in failed: {other}"))),
            }
        }
        Err(e) => Err(Error::auth(format!("token request failed: {e}"))),
    }
}

/// Refresh an existing MSA token with its refresh token.
pub fn refresh(refresh_token: &str) -> Result<MsaToken> {
    let response = agent()
        .post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
        ])
        .map_err(|e| Error::auth(format!("refresh failed: {e}")))?;
    let json: serde_json::Value = serde_json::from_str(&response.into_string().unwrap_or_default())
        .map_err(|e| Error::auth(format!("refresh response was not JSON: {e}")))?;
    Ok(MsaToken {
        access_token: json["access_token"].as_str().unwrap_or_default().to_owned(),
        refresh_token: json["refresh_token"].as_str().map(str::to_owned),
    })
}

/// The three Xbox Live keys used for the proof-of-possession signature.
pub struct XboxKeys {
    p256: EcdsaKeyPair,
    x: Vec<u8>,
    y: Vec<u8>,
}

impl XboxKeys {
    pub fn generate() -> Result<Self> {
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &rand::SystemRandom::new(),
        )
        .map_err(|_| Error::auth("could not generate the Xbox proof key"))?;
        let p256 = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rand::SystemRandom::new(),
        )
        .map_err(|_| Error::auth("bad Xbox proof key"))?;
        // Uncompressed point: 0x04 || X(32) || Y(32).
        let (x, y) = {
            let point = p256.public_key();
            let point = point.as_ref();
            if point.len() != 65 {
                return Err(Error::auth("unexpected P-256 public key length"));
            }
            (point[1..33].to_vec(), point[33..65].to_vec())
        };
        Ok(XboxKeys { p256, x, y })
    }

    fn proof_key(&self) -> serde_json::Value {
        serde_json::json!({
            "crv": "P-256",
            "alg": "ES256",
            "use": "sig",
            "kty": "EC",
            "x": B64URL.encode(&self.x),
            "y": B64URL.encode(&self.y),
        })
    }

    /// The Xbox `Signature` header for a request: the policy blob signed with
    /// SHA-256/ECDSA (already JOSE `r||s` from ring).
    fn signature_header(
        &self,
        method: &str,
        path_and_query: &str,
        authorization: &str,
        body: &str,
    ) -> Result<String> {
        let timestamp = windows_timestamp();
        let mut to_sign = Vec::new();
        to_sign.extend_from_slice(&[0, 0, 0, 1, 0]);
        to_sign.extend_from_slice(&timestamp.to_be_bytes());
        to_sign.push(0);
        to_sign.extend_from_slice(method.as_bytes());
        to_sign.push(0);
        to_sign.extend_from_slice(path_and_query.as_bytes());
        to_sign.push(0);
        to_sign.extend_from_slice(authorization.as_bytes());
        to_sign.push(0);
        to_sign.extend_from_slice(body.as_bytes());
        to_sign.push(0);
        let sig = self
            .p256
            .sign(&rand::SystemRandom::new(), &to_sign)
            .map_err(|_| Error::auth("Xbox signature failed"))?;
        let mut header = Vec::new();
        header.extend_from_slice(&[0, 0, 0, 1]);
        header.extend_from_slice(&timestamp.to_be_bytes());
        header.extend_from_slice(sig.as_ref());
        Ok(B64.encode(header))
    }
}

fn windows_timestamp() -> u64 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    (unix + 11_644_473_600) * 10_000_000
}

fn post_json(
    url: &str,
    body: &serde_json::Value,
    extra: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let mut req = agent()
        .post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    for (k, v) in extra {
        req = req.set(k, v);
    }
    let response = req
        .send_string(&body.to_string())
        .map_err(|e| Error::auth(format!("POST {url} failed: {e}")))?;
    serde_json::from_str(&response.into_string().unwrap_or_default())
        .map_err(|e| Error::auth(format!("POST {url} returned invalid JSON: {e}")))
}

/// A completed Bedrock login: the chain JSON and skin JSON to send in `Login`,
/// and the identity/xuid/name the server will see.
pub struct LoginData {
    pub identity: IdentityKey,
    /// The X25519 key whose public half the server uses for the handshake.
    pub handshake: HandshakeKey,
    pub chain_json: String,
    pub skin_json: String,
    pub xuid: String,
    pub uuid: [u8; 16],
    pub display_name: String,
}

const MULTIPLAYER_RELYING_PARTY: &str = "https://multiplayer.minecraft.net/";
const PLAYFAB_RELYING_PARTY: &str = "https://b980a380.minecraft.playfabapi.com/";

/// A self-signed multiplayer token for offline logins (`AuthenticationType` 2).
/// The server reads the player identity from it and binds `cpk` to the NetherNet
/// transport identity, so `key` must be the session's identity key.
pub fn self_signed_token(key: &IdentityKey, xuid: &str, name: &str, uuid: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cpk = crate::crypto::B64.encode(key.public_der());
    let claims = serde_json::json!({
        "cpk": cpk,
        "xid": xuid,
        "xname": name,
        "identity": uuid,
        "leguuid": uuid,
        "mid": "",
        "iss": "self",
        "aud": "api://auth-minecraft-services/multiplayer",
        "iat": now,
        "exp": now + 3600,
    });
    let header = format!(r#"{{"alg":"ES384","x5u":"{}"}}"#, cpk);
    key.sign_jwt(&header, &claims.to_string())
}

/// The modern (1.21.90+) Bedrock `Login` authInfo: the Xbox chain plus the
/// multiplayer token, with `AuthenticationType` Full.
pub fn auth_info(chain_json: &str, multiplayer_token: &str, authentication_type: i32) -> String {
    serde_json::json!({
        "AuthenticationType": authentication_type,
        "Certificate": format!("{chain_json}\n"),
        "Token": multiplayer_token,
    })
    .to_string()
}

/// The `XBL3.0 x=<uhs>;<token>` header for the PlayFab relying party. PlayFab
/// login (then `session/start`, then `multiplayer/session/start`) mints the
/// NetherNet identity token from it.
pub fn playfab_xbl3(msa_access_token: &str) -> Result<String> {
    let (token, uhs) = xsts_for(msa_access_token, PLAYFAB_RELYING_PARTY)?;
    Ok(format!("XBL3.0 x={uhs};{token}"))
}

/// Acquire an XSTS token for `relying_party`, returning `(token, uhs)`.
fn xsts_for(msa_access_token: &str, relying_party: &str) -> Result<(String, String)> {
    let keys = XboxKeys::generate()?;
    let proof_key = keys.proof_key();

    // --- user token ------------------------------------------------------
    let user_body = serde_json::json!({
        "RelyingParty": "http://auth.xboxlive.com",
        "TokenType": "JWT",
        "Properties": {
            "AuthMethod": "RPS",
            "SiteName": "user.auth.xboxlive.com",
            "RpsTicket": format!("t={msa_access_token}"),
            "ProofKey": proof_key,
        }
    });
    let user_sig =
        keys.signature_header("POST", "/user/authenticate", "", &user_body.to_string())?;
    let user = post_json(
        USER_AUTH_URL,
        &user_body,
        &[("x-xbl-contract-version", "1"), ("Signature", &user_sig)],
    )?;
    let user_token = user["Token"]
        .as_str()
        .ok_or_else(|| Error::auth("no user token"))?
        .to_owned();

    // --- device token ----------------------------------------------------
    let device_body = serde_json::json!({
        "RelyingParty": "http://auth.xboxlive.com",
        "TokenType": "JWT",
        "Properties": {
            "AuthMethod": "ProofOfPossession",
            "DeviceType": "Android",
            "Id": uuid_string(random_uuid()),
            "SerialNumber": uuid_string(random_uuid()),
            "Version": "0.0.0.0",
            "ProofKey": proof_key,
        }
    });
    let device_sig =
        keys.signature_header("POST", "/device/authenticate", "", &device_body.to_string())?;
    let device = post_json(
        DEVICE_AUTH_URL,
        &device_body,
        &[("x-xbl-contract-version", "1"), ("Signature", &device_sig)],
    )?;
    let device_token = device["Token"]
        .as_str()
        .ok_or_else(|| Error::auth("no device token"))?
        .to_owned();

    // --- XSTS (user + device; title tokens are for title-based auth) -----
    let xsts_body = serde_json::json!({
        "RelyingParty": relying_party,
        "TokenType": "JWT",
        "Properties": {
            "DeviceToken": device_token,
            "UserTokens": [user_token],
            "SandboxId": "RETAIL",
            "ProofKey": proof_key,
        }
    });
    let xsts_sig = keys.signature_header("POST", "/xsts/authorize", "", &xsts_body.to_string())?;
    let xsts = post_json(
        XSTS_AUTH_URL,
        &xsts_body,
        &[("x-xbl-contract-version", "1"), ("Signature", &xsts_sig)],
    )?;
    let xsts_token = xsts["Token"]
        .as_str()
        .ok_or_else(|| Error::auth("no XSTS token"))?
        .to_owned();
    let uhs = xsts["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .ok_or_else(|| Error::auth("no XSTS uhs"))?
        .to_owned();
    Ok((xsts_token, uhs))
}

/// Run the Xbox Live chain and build the login payloads.
///
/// `server_address` is the address the player typed; it is echoed in the skin
/// JWT (servers compare it). `client_version` is the Bedrock version string.
pub fn login(
    msa_access_token: &str,
    server_address: &str,
    client_version: &str,
) -> Result<LoginData> {
    login_with_identity(
        msa_access_token,
        server_address,
        client_version,
        IdentityKey::generate()?,
    )
}

/// Like [`login`], but with a caller-supplied identity key, so the same key can
/// back both the chain's `identityPublicKey` and the multiplayer token's `cpk`.
pub fn login_with_identity(
    msa_access_token: &str,
    server_address: &str,
    client_version: &str,
    identity: IdentityKey,
) -> Result<LoginData> {
    let (xsts_token, uhs) = xsts_for(msa_access_token, MULTIPLAYER_RELYING_PARTY)?;

    // --- Minecraft chain -------------------------------------------------
    let identity_public = B64.encode(identity.public_der());
    let chain_request = serde_json::json!({ "identityPublicKey": identity_public });
    let chain = post_json(
        MINECRAFT_AUTH_URL,
        &chain_request,
        &[
            ("Authorization", &format!("XBL3.0 x={uhs};{xsts_token}")),
            ("User-Agent", "MCPE/Android"),
            ("Client-Version", client_version),
        ],
    )?;
    let inherited = chain["chain"]
        .as_array()
        .ok_or_else(|| Error::auth("Minecraft chain response has no chain"))?
        .clone();
    if inherited.is_empty() {
        return Err(Error::auth("Minecraft returned an empty chain"));
    }

    // The first inherited JWT carries the root key; our self-signed JWT points
    // `identityPublicKey` at it so the chain validates.
    let first_header = jwt_part(inherited[0].as_str().unwrap_or_default(), 0)?;
    let first_x5u = first_header["x5u"].as_str().unwrap_or_default().to_owned();
    let now = now_secs();
    let self_signed_payload = serde_json::json!({
        "certificateAuthority": true,
        "exp": now + 6 * 3600,
        "identityPublicKey": first_x5u,
        "nbf": now.saturating_sub(6 * 3600),
    });
    let self_signed = identity.sign_jwt(
        &serde_json::json!({ "alg": "ES384", "x5u": identity_public }).to_string(),
        &self_signed_payload.to_string(),
    )?;

    let mut full = vec![serde_json::Value::String(self_signed)];
    full.extend(inherited.iter().cloned());

    // Read extraData (XUID, identity, displayName) from the last inherited JWT.
    let last_payload = jwt_part(inherited.last().unwrap().as_str().unwrap_or_default(), 1)?;
    let extra = &last_payload["extraData"];
    let xuid = extra["XUID"].as_str().unwrap_or_default().to_owned();
    let display_name = extra["displayName"].as_str().unwrap_or_default().to_owned();
    let uuid = parse_uuid(extra["identity"].as_str().unwrap_or_default()).unwrap_or([0u8; 16]);

    let handshake = HandshakeKey::generate()?;
    let chain_json = serde_json::json!({ "chain": full }).to_string();
    let skin_json = skin_jwt(
        &identity,
        &handshake.public_der(),
        server_address,
        client_version,
        &display_name,
    )?;

    Ok(LoginData {
        identity,
        handshake,
        chain_json,
        skin_json,
        xuid,
        uuid,
        display_name,
    })
}

fn jwt_part(jwt: &str, index: usize) -> Result<serde_json::Value> {
    let part = jwt
        .split('.')
        .nth(index)
        .ok_or_else(|| Error::auth("JWT is missing a part"))?;
    let bytes = B64URL
        .decode(part)
        .or_else(|_| B64.decode(part))
        .map_err(|_| Error::auth("JWT part is not base64"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::auth("JWT part is not JSON"))
}

/// Build the client-data (skin) JWT, signed with the identity key.
fn skin_jwt(
    identity: &IdentityKey,
    handshake_public_der: &[u8],
    server_address: &str,
    client_version: &str,
    display_name: &str,
) -> Result<String> {
    // The client-data JWT's `x5u` is the X25519 handshake key (the server reads
    // it to derive the shared secret); the signature is the ES384 identity key.
    let header = serde_json::json!({
        "alg": "ES384",
        "x5u": B64.encode(handshake_public_der),
    });
    let random_id = random_i64();
    let skin = serde_json::json!({
        "AnimatedImageData": [],
        "ArmSize": "wide",
        "CapeData": "",
        "CapeId": "",
        "CapeImageHeight": 0,
        "CapeImageWidth": 0,
        "CapeOnClassicSkin": false,
        "ClientRandomId": random_id,
        "CurrentInputMode": 1,
        "DefaultInputMode": 1,
        "DeviceId": uuid_string(random_uuid()),
        "DeviceModel": crate::DEVICE_MODEL,
        "DeviceOS": crate::DEVICE_OS,
        "GameVersion": client_version,
        "GuiScale": 0,
        "LanguageCode": "en_US",
        "PersonaPieces": [],
        "PersonaSkin": false,
        "PieceTintColors": [],
        "PlatformOfflineId": "",
        "PlatformOnlineId": "",
        "PlayFabId": "",
        "PremiumSkin": false,
        "SelfSignedId": uuid_string(random_uuid()),
        "ServerAddress": server_address,
        "SkinAnimationData": "",
        "SkinColor": "#0",
        "SkinData": crate::mapping::steve_skin_b64(),
        "SkinGeometryData": B64.encode(crate::mapping::HUMANOID_GEOMETRY),
        "SkinGeometryDataEngineVersion": "",
        "SkinId": format!("{}.Custom", uuid_string(random_uuid())),
        "SkinImageHeight": 64,
        "SkinImageWidth": 64,
        "SkinResourcePatch": "ewogICAiZ2VvbWV0cnkiIDogewogICAgICAiZGVmYXVsdCIgOiAiZ2VvbWV0cnkuaHVtYW5vaWQuY3VzdG9tIgogICB9Cn0K",
        "ThirdPartyName": display_name,
        "ThirdPartyNameOnly": false,
        "UIProfile": 0,
    });
    identity.sign_jwt(&header.to_string(), &skin.to_string())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn random_uuid() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let rng = rand::SystemRandom::new();
    let _ = ring::rand::SecureRandom::fill(&rng, &mut bytes);
    // Set the version/variant bits so it looks like a v4 UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

fn random_i64() -> i64 {
    let mut bytes = [0u8; 8];
    let rng = rand::SystemRandom::new();
    let _ = ring::rand::SecureRandom::fill(&rng, &mut bytes);
    i64::from_le_bytes(bytes).abs()
}

pub fn uuid_string(u: [u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7], u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_part_decodes_header_and_payload() {
        let key = IdentityKey::generate().unwrap();
        let jwt = key
            .sign_jwt(
                r#"{"alg":"ES384","x5u":"ROOT"}"#,
                r#"{"extraData":{"XUID":"42"}}"#,
            )
            .unwrap();
        assert_eq!(jwt_part(&jwt, 0).unwrap()["x5u"], "ROOT");
        assert_eq!(jwt_part(&jwt, 1).unwrap()["extraData"]["XUID"], "42");
    }

    #[test]
    fn proof_key_is_32_byte_pairs() {
        let keys = XboxKeys::generate().unwrap();
        assert_eq!(keys.x.len(), 32);
        assert_eq!(keys.y.len(), 32);
    }

    #[test]
    fn uuids_round_trip() {
        let u = random_uuid();
        assert_eq!(parse_uuid(&uuid_string(u)).unwrap(), u);
    }
}
