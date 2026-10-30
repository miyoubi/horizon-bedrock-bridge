//! The single data-channel SDP media section NetherNet peers exchange, modelled
//! as plain text (no WebRTC crate needed). The ICE/DTLS/SCTP session fills in the
//! ufrag/pwd and certificate fingerprint; this module writes and reads the SDP,
//! and carries the session-level `a=identity` value.
//!
//! The shape mirrors the `nethernet` reference's `protocol::webrtc::Description`.

use crate::error::{Error, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// The SCTP data-channel port.
pub const SCTP_PORT: u16 = 5000;
/// `max-message-size` assumed when the remote omits it.
pub const DEFAULT_SCTP_MAX_MESSAGE_SIZE: u32 = 65536;

/// DTLS setup role. Server announces `actpass`; Client and Auto announce `active`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtlsRole {
    Client,
    Server,
    Auto,
}

impl DtlsRole {
    fn setup(self) -> &'static str {
        match self {
            DtlsRole::Server => "actpass",
            DtlsRole::Client | DtlsRole::Auto => "active",
        }
    }
}

/// A peer's ICE credentials, DTLS setup and certificate fingerprint.
#[derive(Clone, Debug, PartialEq)]
pub struct Description {
    pub ufrag: String,
    pub pwd: String,
    pub dtls_role: DtlsRole,
    /// Algorithm and digest from the `fingerprint` attribute.
    pub fingerprint: (String, String),
    pub sctp_max_message_size: u32,
    /// Raw session-level `a=identity` value (opaque here).
    pub identity: Option<String>,
}

impl Description {
    /// Encode with `ice-options:trickle` and no candidates.
    pub fn encode_trickle(&self) -> String {
        self.encode(&[], true)
    }

    /// Encode with embedded candidate lines (`candidate:...` values) and
    /// `end-of-candidates`.
    pub fn encode_full(&self, candidates: &[String]) -> String {
        self.encode(candidates, false)
    }

    fn encode(&self, candidates: &[String], trickle: bool) -> String {
        let mut s = String::new();
        s.push_str("v=0\r\n");
        s.push_str(&format!("o=- {} 2 IN IP4 127.0.0.1\r\n", session_id()));
        s.push_str("s=-\r\n");
        s.push_str("t=0 0\r\n");
        s.push_str("a=group:BUNDLE 0\r\n");
        s.push_str("a=extmap-allow-mixed\r\n");
        s.push_str("a=msid-semantic: WMS\r\n");
        if let Some(identity) = &self.identity {
            s.push_str(&format!("a=identity:{identity}\r\n"));
        }
        s.push_str("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n");
        s.push_str("c=IN IP4 0.0.0.0\r\n");
        s.push_str(&format!("a=ice-ufrag:{}\r\n", self.ufrag));
        s.push_str(&format!("a=ice-pwd:{}\r\n", self.pwd));
        s.push_str(&format!(
            "a=fingerprint:{} {}\r\n",
            self.fingerprint.0, self.fingerprint.1
        ));
        s.push_str(&format!("a=setup:{}\r\n", self.dtls_role.setup()));
        s.push_str("a=mid:0\r\n");
        s.push_str(&format!("a=sctp-port:{SCTP_PORT}\r\n"));
        s.push_str(&format!(
            "a=max-message-size:{}\r\n",
            self.sctp_max_message_size
        ));
        for (index, candidate) in candidates.iter().enumerate() {
            // Candidates are given as `candidate:...`; append the trailing
            // generation/ufrag/network attributes the wire expects.
            s.push_str(&format!(
                "a={} generation 0 ufrag {} network-id {} network-cost 0\r\n",
                candidate.trim_end(),
                self.ufrag,
                index
            ));
        }
        if trickle {
            s.push_str("a=ice-options:trickle\r\n");
        } else {
            s.push_str("a=end-of-candidates\r\n");
        }
        s
    }

    /// Parse a description; needs exactly one media section. Unparsable
    /// candidate lines are skipped.
    pub fn parse(sdp: &str) -> Result<(Description, Vec<String>)> {
        let lines: Vec<&str> = sdp.split(['\r', '\n']).collect();
        let media_count = lines.iter().filter(|l| l.starts_with("m=")).count();
        if media_count != 1 {
            return Err(Error::decode(format!(
                "SDP has {media_count} media sections, expected 1"
            )));
        }

        let attr = |key: &str| -> Result<&str> {
            lines
                .iter()
                .find_map(|l| l.strip_prefix(key))
                .ok_or_else(|| Error::decode(format!("SDP missing {key}")))
        };

        let ufrag = attr("a=ice-ufrag:")?.trim().to_string();
        let pwd = attr("a=ice-pwd:")?.trim().to_string();

        let fingerprint = attr("a=fingerprint:")?.trim();
        let (algorithm, value) = fingerprint
            .split_once(' ')
            .ok_or_else(|| Error::decode(format!("invalid fingerprint: {fingerprint}")))?;

        let setup = attr("a=setup:")?.trim();
        let dtls_role = match setup {
            "active" => DtlsRole::Client,
            "actpass" => DtlsRole::Server,
            other => return Err(Error::decode(format!("invalid setup: {other}"))),
        };

        let sctp_max_message_size = attr("a=max-message-size:")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_SCTP_MAX_MESSAGE_SIZE);

        let candidates: Vec<String> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("a=candidate:"))
            .map(|v| format!("candidate:{}", v.trim_end()))
            .collect();

        let identity = lines
            .iter()
            .find_map(|l| l.strip_prefix("a=identity:"))
            .map(|v| v.trim().to_string());

        Ok((
            Description {
                ufrag,
                pwd,
                dtls_role,
                fingerprint: (algorithm.to_string(), value.to_string()),
                sctp_max_message_size,
                identity,
            },
            candidates,
        ))
    }
}

fn session_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn description(role: DtlsRole) -> Description {
        Description {
            ufrag: "abcd".into(),
            pwd: "0123456789abcdef01234567".into(),
            dtls_role: role,
            fingerprint: ("sha-256".into(), "AB:CD:EF".into()),
            sctp_max_message_size: 65536,
            identity: None,
        }
    }

    #[test]
    fn trickle_round_trips() {
        let encoded = description(DtlsRole::Server).encode_trickle();
        assert!(encoded.contains("a=setup:actpass"));
        assert!(encoded.contains("a=sctp-port:5000"));
        assert!(encoded.contains("a=max-message-size:65536"));
        assert!(encoded.contains("m=application 9 UDP/DTLS/SCTP webrtc-datachannel"));
        assert!(encoded.contains("a=ice-options:trickle"));
        assert!(!encoded.contains("a=candidate:"));

        let (parsed, candidates) = Description::parse(&encoded).unwrap();
        assert_eq!(parsed.ufrag, "abcd");
        assert_eq!(parsed.pwd, "0123456789abcdef01234567");
        assert_eq!(parsed.dtls_role, DtlsRole::Server);
        assert_eq!(parsed.fingerprint, ("sha-256".into(), "AB:CD:EF".into()));
        assert!(candidates.is_empty());
    }

    #[test]
    fn full_embeds_candidates_and_end_of_candidates() {
        let candidates =
            vec!["candidate:1 1 udp 2130706431 192.168.1.10 54321 typ host".to_string()];
        let encoded = description(DtlsRole::Server).encode_full(&candidates);
        assert!(encoded.contains("a=candidate:"));
        assert!(encoded.contains("192.168.1.10"));
        assert!(encoded.contains("a=end-of-candidates"));
        assert!(!encoded.contains("a=ice-options:trickle"));

        let (_, parsed) = Description::parse(&encoded).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].starts_with("candidate:"));
        assert!(parsed[0].contains("192.168.1.10"));
    }

    #[test]
    fn auto_and_client_are_active() {
        let encoded = description(DtlsRole::Auto).encode_trickle();
        assert!(encoded.contains("a=setup:active"));
        assert_eq!(
            Description::parse(&encoded).unwrap().0.dtls_role,
            DtlsRole::Client
        );
    }

    #[test]
    fn missing_fingerprint_is_rejected() {
        let encoded = description(DtlsRole::Server)
            .encode_trickle()
            .lines()
            .filter(|l| !l.starts_with("a=fingerprint"))
            .map(|l| format!("{l}\r\n"))
            .collect::<String>();
        assert!(Description::parse(&encoded).is_err());
    }

    #[test]
    fn identity_is_session_level_and_round_trips() {
        let mut d = description(DtlsRole::Server);
        d.identity = Some("opaque-identity-blob".into());
        let encoded = d.encode_trickle();
        let identity_pos = encoded.find("a=identity:opaque-identity-blob").unwrap();
        let media_pos = encoded.find("m=application").unwrap();
        assert!(identity_pos < media_pos);
        let (parsed, _) = Description::parse(&encoded).unwrap();
        assert_eq!(parsed.identity.as_deref(), Some("opaque-identity-blob"));
    }

    #[test]
    fn two_media_sections_are_rejected() {
        let mut two = description(DtlsRole::Server).encode_trickle();
        two.push_str("m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n");
        assert!(Description::parse(&two).is_err());
    }
}
