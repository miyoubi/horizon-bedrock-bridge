//! The [`Transport`] adapter that lets [`crate::client::BedrockClient`] run its
//! login state machine over NetherNet: batches are segmented into reliable
//! data-channel messages, and incoming segments are reassembled.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::client::Transport;
use crate::error::{Error, Result};
use crate::nethernet::identity::ClientIdentity;
use crate::nethernet::message::{Message, MessageSegment};
use crate::nethernet::webrtc::Session;

/// A live NetherNet session presented as a byte transport.
pub struct NetherNetTransport {
    session: Session,
    reassembly: Message,
    pending: VecDeque<Vec<u8>>,
}

impl NetherNetTransport {
    /// Join `network_id` at `base` and wait for the reliable channel to open.
    pub fn connect(base: &str, network_id: &str, identity: &ClientIdentity) -> Result<Self> {
        crate::nethernet::trace(format!("joining {base} as network {network_id}"));
        let mut session = Session::connect(base, network_id, identity)?;
        let deadline = Instant::now() + Duration::from_secs(25);
        while !session.is_ready() && Instant::now() < deadline {
            session.pump(Duration::from_millis(250))?;
        }
        if !session.is_ready() {
            crate::nethernet::trace("data channel did not open within 25s");
            return Err(Error::transport("NetherNet: data channel did not open"));
        }
        crate::nethernet::trace("data channel open");
        Ok(Self {
            session,
            reassembly: Message::new(),
            pending: VecDeque::new(),
        })
    }
}

impl Transport for NetherNetTransport {
    fn send(&mut self, body: &[u8]) -> Result<()> {
        for segment in Message::split_into_segments(body)? {
            let data = segment.encode();
            crate::nethernet::trace(format!("send segment {} bytes", data.len()));
            // The connection can report Connected a moment before SCTP marks the
            // channel OPEN; retry while it settles.
            let mut attempts = 0;
            loop {
                match self.session.send_reliable(&data) {
                    Ok(()) => break,
                    Err(e) if attempts < 100 => {
                        attempts += 1;
                        self.session.pump(Duration::from_millis(20))?;
                        if attempts == 100 {
                            return Err(e);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(Some(message));
        }
        for raw in self.session.pump(Duration::from_millis(20))? {
            crate::nethernet::trace(format!(
                "received {} bytes on reliable channel {:02x?}",
                raw.len(),
                &raw[..raw.len().min(24)]
            ));
            let Ok(segment) = MessageSegment::decode(&raw) else {
                continue;
            };
            if let Ok(Some(message)) = self.reassembly.add_segment(segment) {
                self.pending.push_back(message);
            }
        }
        Ok(self.pending.pop_front())
    }

    fn close(&mut self) {
        self.session.close();
    }

    fn flush(&mut self) {
        // `send_reliable` only queues into the peer connection; the pump moves
        // it onto the wire.
        let _ = self.session.pump(Duration::from_millis(150));
    }
}
