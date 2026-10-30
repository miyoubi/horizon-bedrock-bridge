//! NetherNet HTTP join: send an SDP offer (already carrying the identity
//! assertion) to `POST /v1/join/<network_id>` and read the SDP answer back.
//!
//! The body is `application/sdp`; a 2xx body is the answer, unless it parses as
//! a bare number, which is a rejection code. The offer itself comes from the
//! WebRTC peer connection (see [`crate::nethernet`] note on `rtc`).

use crate::error::{Error, Result};

use super::http::{CLIENT_USER_AGENT, CONTENT_TYPE, JOIN_PATH};

/// Largest accepted answer.
pub const MAX_ANSWER_SIZE: usize = 1 << 20;

/// `/v1/join/<network_id>`.
pub fn join_url(base: &str, network_id: &str) -> String {
    format!("{}{}/{}", base.trim_end_matches('/'), JOIN_PATH, network_id)
}

/// Why a join response was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinRejection {
    /// A non-2xx status.
    Status(u16),
    /// A 2xx body with no answer.
    Empty,
    /// An answer larger than [`MAX_ANSWER_SIZE`].
    TooLarge(usize),
    /// A 2xx body that is a signaling error code.
    Rejected(u32),
}

impl std::fmt::Display for JoinRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JoinRejection::Status(s) => write!(f, "join answered with status {s}"),
            JoinRejection::Empty => write!(f, "join sent no answer"),
            JoinRejection::TooLarge(n) => write!(f, "answer of {n} bytes exceeds the limit"),
            JoinRejection::Rejected(c) => write!(f, "server rejected the offer with code {c}"),
        }
    }
}

/// Validate a join response without sending anything.
pub fn validate_join_response(status: u16, body: &str) -> std::result::Result<(), JoinRejection> {
    if !(200..300).contains(&status) {
        return Err(JoinRejection::Status(status));
    }
    if body.is_empty() {
        return Err(JoinRejection::Empty);
    }
    if body.len() > MAX_ANSWER_SIZE {
        return Err(JoinRejection::TooLarge(body.len()));
    }
    if let Ok(code) = body.trim().parse::<u32>() {
        return Err(JoinRejection::Rejected(code));
    }
    Ok(())
}

/// Send `offer_sdp` to `base` for `network_id` and return the SDP answer.
///
/// `base` is the server's HTTP origin (e.g. `http://127.0.0.1:19132`).
pub fn join(base: &str, network_id: &str, offer_sdp: &str) -> Result<String> {
    let url = join_url(base, network_id);
    let response = ureq::post(&url)
        .set("Content-Type", CONTENT_TYPE)
        .set("User-Agent", CLIENT_USER_AGENT)
        .send_string(offer_sdp);

    let (status, body) = match response {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(ureq::Error::Transport(t)) => return Err(Error::transport(format!("join {url}: {t}"))),
    };

    validate_join_response(status, &body).map_err(|why| Error::Refused(why.to_string()))?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_names_the_network_under_v1_join() {
        assert_eq!(
            join_url("http://127.0.0.1:19132/", "abc"),
            "http://127.0.0.1:19132/v1/join/abc"
        );
        assert_eq!(
            join_url("http://host:19132", "5c4c"),
            "http://host:19132/v1/join/5c4c"
        );
    }

    #[test]
    fn a_2xx_answer_is_accepted() {
        assert_eq!(validate_join_response(200, "v=0\r\n"), Ok(()));
        assert_eq!(validate_join_response(204, "v=0"), Ok(()));
    }

    #[test]
    fn a_non_2xx_status_is_refused() {
        assert_eq!(
            validate_join_response(500, "v=0"),
            Err(JoinRejection::Status(500))
        );
    }

    #[test]
    fn an_empty_answer_is_refused() {
        assert_eq!(validate_join_response(200, ""), Err(JoinRejection::Empty));
    }

    #[test]
    fn a_bare_number_is_a_rejection_code() {
        assert_eq!(
            validate_join_response(200, "4"),
            Err(JoinRejection::Rejected(4))
        );
        assert_eq!(
            validate_join_response(200, " 7\n"),
            Err(JoinRejection::Rejected(7))
        );
    }

    #[test]
    fn an_oversized_answer_is_refused() {
        let body = "a".repeat(MAX_ANSWER_SIZE + 1);
        assert_eq!(
            validate_join_response(200, &body),
            Err(JoinRejection::TooLarge(body.len()))
        );
    }
}
