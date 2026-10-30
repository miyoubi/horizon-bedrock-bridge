//! The Minecraft authorization-service exchange that yields a NetherNet identity
//! token (ported from RaphiMC's MinecraftAuth, `BedrockAuthManager`):
//!
//! 1. an XSTS token for the PlayFab relying party → `XBL3.0 x=<uhs>;<token>`,
//! 2. PlayFab `Client/LoginWithXbox` → `SessionTicket`,
//! 3. `POST /api/v1.0/session/start` (PlayFab ticket) → `authorizationHeader`,
//! 4. `POST /api/v1.0/multiplayer/session/start` with our P-384 public key →
//!    `signedToken` (the RS256 identity token whose `cpk` is our key).
//!
//! The signed token replaces the self-signed one in the NetherNet identity
//! assertion; the fingerprint signature still uses the same P-384 key.

use base64::Engine;
use serde_json::json;

use crate::crypto::B64;
use crate::error::{Error, Result};

/// The Bedrock PlayFab title id.
pub const PLAYFAB_TITLE_ID: &str = "20CA2";
/// `session/start`.
pub const SESSION_START: &str =
    "https://authorization.franchise.minecraft-services.net/api/v1.0/session/start";
/// `multiplayer/session/start`.
pub const MULTIPLAYER_START: &str =
    "https://authorization.franchise.minecraft-services.net/api/v1.0/multiplayer/session/start";

fn post_json(
    url: &str,
    body: &serde_json::Value,
    headers: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let mut req = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("User-Agent", "MCPE/Android");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let response = req
        .send_string(&body.to_string())
        .map_err(|e| Error::auth(format!("POST {url}: {e}")))?;
    serde_json::from_str(&response.into_string().unwrap_or_default())
        .map_err(|e| Error::auth(format!("POST {url} returned invalid JSON: {e}")))
}

/// PlayFab `Client/LoginWithXbox`, returning the session ticket.
pub fn playfab_login(xbl3: &str) -> Result<String> {
    let url = format!(
        "https://{}.playfabapi.com/Client/LoginWithXbox",
        PLAYFAB_TITLE_ID.to_lowercase()
    );
    let body = json!({
        "CreateAccount": true,
        "InfoRequestParameters": { "GetPlayerProfile": true, "GetUserAccountInfo": true },
        "TitleId": PLAYFAB_TITLE_ID,
        "XboxToken": xbl3,
    });
    let json = post_json(&url, &body, &[])?;
    json["data"]["SessionTicket"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::auth("PlayFab login: no SessionTicket"))
}

/// `session/start`, returning the authorization header for the next call.
pub fn session_start(session_ticket: &str, game_version: &str) -> Result<String> {
    let body = json!({
        "device": {
            "applicationType": "MinecraftPE",
            "gameVersion": game_version,
            "id": random_device_id(),
            "memory": 32u64 * 1024 * 1024 * 1024,
            "hardwareMemoryTier": 5,
            "platform": "Windows10",
            "playFabTitleId": PLAYFAB_TITLE_ID,
            "storePlatform": "uwp.store",
            "type": "Windows10",
        },
        "user": {
            "language": "en",
            "regionCode": "US",
            "languageCode": "en-US",
            "tokenType": "PlayFab",
            "token": session_ticket,
        },
    });
    let json = post_json(SESSION_START, &body, &[])?;
    json["result"]["authorizationHeader"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::auth("session/start: no authorizationHeader"))
}

/// `multiplayer/session/start`, returning the signed identity token.
pub fn multiplayer_token(authorization_header: &str, public_key_der: &[u8]) -> Result<String> {
    let body = json!({ "publicKey": B64.encode(public_key_der) });
    let json = post_json(
        MULTIPLAYER_START,
        &body,
        &[("Authorization", authorization_header)],
    )?;
    json["result"]["signedToken"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::auth("multiplayer/session/start: no signedToken"))
}

/// A 32-hex-digit device id.
fn random_device_id() -> String {
    let mut bytes = [0u8; 16];
    let rng = ring::rand::SystemRandom::new();
    let _ = ring::rand::SecureRandom::fill(&rng, &mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_32_hex() {
        let id = random_device_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn public_key_is_base64_standard() {
        // B64 is padded standard base64, what the service expects.
        assert_eq!(B64.encode([1u8, 2, 3]), "AQID");
    }
}
