//! NetherNet signaling messages, in their `TYPE connection_id data` text form.
//!
//! Both LAN discovery and the HTTP endpoint exchange these. The `network_id` is
//! the sender's identity and travels out of band (in the LAN packet or the HTTP
//! body), so it is not part of the text form.

use crate::error::{Error, Result};
use std::fmt;
use std::str::FromStr;

/// Wire names are CONNECTREQUEST, CONNECTRESPONSE, CANDIDATEADD and CONNECTERROR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalType {
    /// An SDP offer.
    Offer,
    /// An SDP answer.
    Answer,
    /// One ICE candidate line.
    Candidate,
    /// A decimal error code.
    Error,
}

impl SignalType {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalType::Offer => "CONNECTREQUEST",
            SignalType::Answer => "CONNECTRESPONSE",
            SignalType::Candidate => "CANDIDATEADD",
            SignalType::Error => "CONNECTERROR",
        }
    }
}

impl FromStr for SignalType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "CONNECTREQUEST" => Ok(SignalType::Offer),
            "CONNECTRESPONSE" => Ok(SignalType::Answer),
            "CANDIDATEADD" => Ok(SignalType::Candidate),
            "CONNECTERROR" => Ok(SignalType::Error),
            other => Err(Error::decode(format!("unknown signal type {other}"))),
        }
    }
}

impl fmt::Display for SignalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One signaling message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
    pub signal_type: SignalType,
    pub connection_id: u64,
    /// SDP for offers/answers, a candidate line, or a decimal error code.
    pub data: String,
    /// The remote peer's id (out of band).
    pub network_id: String,
}

impl Signal {
    pub fn new(
        signal_type: SignalType,
        connection_id: u64,
        data: impl Into<String>,
        network_id: impl Into<String>,
    ) -> Self {
        Self {
            signal_type,
            connection_id,
            data: data.into(),
            network_id: network_id.into(),
        }
    }

    pub fn offer(
        connection_id: u64,
        sdp: impl Into<String>,
        network_id: impl Into<String>,
    ) -> Self {
        Self::new(SignalType::Offer, connection_id, sdp, network_id)
    }
    pub fn answer(
        connection_id: u64,
        sdp: impl Into<String>,
        network_id: impl Into<String>,
    ) -> Self {
        Self::new(SignalType::Answer, connection_id, sdp, network_id)
    }
    pub fn candidate(
        connection_id: u64,
        candidate: impl Into<String>,
        network_id: impl Into<String>,
    ) -> Self {
        Self::new(SignalType::Candidate, connection_id, candidate, network_id)
    }
    pub fn error(connection_id: u64, code: u32, network_id: impl Into<String>) -> Self {
        Self::new(
            SignalType::Error,
            connection_id,
            code.to_string(),
            network_id,
        )
    }

    /// Parse `TYPE id data`, with `network_id` supplied by the caller.
    pub fn from_string(text: &str, network_id: impl Into<String>) -> Result<Self> {
        let mut parts = text.splitn(3, ' ');
        let kind = parts.next().ok_or_else(|| Error::decode("empty signal"))?;
        let id = parts
            .next()
            .ok_or_else(|| Error::decode("signal has no connection id"))?;
        let data = parts
            .next()
            .ok_or_else(|| Error::decode("signal has no data"))?;
        Ok(Self {
            signal_type: SignalType::from_str(kind)?,
            connection_id: id
                .parse()
                .map_err(|e| Error::decode(format!("signal connection id: {e}")))?,
            data: data.to_owned(),
            network_id: network_id.into(),
        })
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {}",
            self.signal_type.as_str(),
            self.connection_id,
            self.data
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_round_trips() {
        let s = Signal::offer(42, "v=0\r\nsdp", "net-1");
        assert_eq!(s.to_string(), "CONNECTREQUEST 42 v=0\r\nsdp");
        let back = Signal::from_string(&s.to_string(), "net-1").unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn error_code_is_decimal() {
        let s = Signal::error(7, 3, "n");
        assert_eq!(s.data, "3");
        assert_eq!(Signal::from_string("CONNECTERROR 7 3", "n").unwrap(), s);
    }

    #[test]
    fn malformed_signals_are_rejected() {
        assert!(Signal::from_string("", "n").is_err());
        assert!(Signal::from_string("CONNECTREQUEST", "n").is_err());
        assert!(Signal::from_string("CONNECTREQUEST x data", "n").is_err());
        assert!(Signal::from_string("NOPE 1 data", "n").is_err());
    }
}
