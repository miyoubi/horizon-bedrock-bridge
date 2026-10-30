//! NetherNet's HTTP signaling: the join path, content type, and the server
//! status JSON. The join itself is an SDP offer/answer exchange
//! (`application/sdp`) that hands off to the WebRTC session — the portable
//! parts (paths, status, request shapes) live here.
//!
//! Verified against a live 1.26.61 dedicated server's `GET /v1/join`.

use crate::error::{Error, Result};

/// The status/join path; `GET` returns the status, `POST` starts a join.
pub const JOIN_PATH: &str = "/v1/join";
/// The join body content type.
pub const CONTENT_TYPE: &str = "application/sdp";
/// The user agent the vanilla client sends.
pub const CLIENT_USER_AGENT: &str = "libhttpclient/1.0.0.0";

/// `/v1/join/<network_id>` for one server.
pub fn join_path(network_id: &str) -> String {
    format!("{JOIN_PATH}/{network_id}")
}

/// The server status JSON returned by `GET /v1/join`.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub name: String,
    pub protocol: u32,
    pub version: String,
    pub level: String,
    pub players: i32,
    pub max_players: i32,
    pub game_type: u8,
}

impl Status {
    pub fn parse(json: &str) -> Result<Self> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|e| Error::decode(format!("status JSON: {e}")))?;
        let field = |k: &str| {
            v.get(k)
                .ok_or_else(|| Error::decode(format!("status has no {k}")))
        };
        Ok(Status {
            name: field("name")?
                .as_str()
                .ok_or_else(|| Error::decode("status name is not a string"))?
                .to_owned(),
            protocol: field("protocol")?
                .as_u64()
                .ok_or_else(|| Error::decode("status protocol is not a number"))?
                as u32,
            version: field("version")?
                .as_str()
                .ok_or_else(|| Error::decode("status version is not a string"))?
                .to_owned(),
            level: field("level")?
                .as_str()
                .ok_or_else(|| Error::decode("status level is not a string"))?
                .to_owned(),
            players: field("players")?
                .as_i64()
                .ok_or_else(|| Error::decode("status players is not a number"))?
                as i32,
            max_players: field("maxPlayers")?
                .as_i64()
                .ok_or_else(|| Error::decode("status maxPlayers is not a number"))?
                as i32,
            game_type: field("gameType")?
                .as_u64()
                .ok_or_else(|| Error::decode("status gameType is not a number"))?
                as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact body a live Bedrock 1.26.61 dedicated server returned.
    const LIVE: &str = r#"{"name":"Dedicated Server","protocol":2225,"version":"1.26.61","level":"Bedrock level","players":0,"maxPlayers":10,"gameType":0}"#;

    #[test]
    fn parses_live_status() {
        let s = Status::parse(LIVE).unwrap();
        assert_eq!(s.name, "Dedicated Server");
        assert_eq!(s.protocol, 2225);
        assert_eq!(s.version, "1.26.61");
        assert_eq!(s.level, "Bedrock level");
        assert_eq!(s.players, 0);
        assert_eq!(s.max_players, 10);
        assert_eq!(s.game_type, 0);
    }

    #[test]
    fn rejects_missing_fields() {
        assert!(Status::parse(r#"{"name":"x"}"#).is_err());
        assert!(Status::parse("not json").is_err());
    }

    #[test]
    fn join_path_is_v1_join_id() {
        assert_eq!(join_path("abc"), "/v1/join/abc");
    }
}
