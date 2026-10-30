//! NetherNet over WebRTC, driven sans-I/O from a `UdpSocket`.
//!
//! `rtc` is sans-I/O: we own the socket and feed it packets (`handle_read`),
//! drain what it wants sent (`poll_write`), pump timers (`poll_timeout`), and
//! read application messages (`poll_read`). This session:
//!
//! 1. binds a UDP socket and registers its host candidate,
//! 2. opens the two data channels and creates the offer,
//! 3. signs the offer with an identity assertion and POSTs it to `/v1/join`,
//! 4. sets the server's answer and runs the ICE/DTLS/SCTP handshake,
//! 5. relays `ReliableDataChannel` payloads to/from the caller.
//!
//! Feature-gated (`nethernet-webrtc`) because the webrtc tree is heavy.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime};

use bytes::BytesMut;
use rtc::data_channel::RTCDataChannelId;
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::event::{RTCDataChannelEvent, RTCPeerConnectionEvent};
use rtc::peer_connection::message::{RTCMessage, TaggedRTCMessage};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::peer_connection::transport::{CandidateConfig, CandidateHostConfig, RTCIceCandidate};
use rtc::peer_connection::RTCPeerConnectionBuilder;
use rtc::sansio::Protocol;
use rtc::shared::{TaggedBytesMut, TransportContext, TransportProtocol};

use crate::error::{Error, Result};
use crate::nethernet::identity::ClientIdentity;

/// The reliable data channel label.
pub const RELIABLE_CHANNEL: &str = "ReliableDataChannel";
/// The unreliable data channel label.
pub const UNRELIABLE_CHANNEL: &str = "UnreliableDataChannel";

/// A live NetherNet session: the peer connection plus the socket it rides on.
pub struct Session {
    pc: rtc::peer_connection::RTCPeerConnection,
    socket: UdpSocket,
    local_addr: SocketAddr,
    reliable: Option<RTCDataChannelId>,
    ready: bool,
}

impl Session {
    /// Perform the offer/join/answer handshake and start the transport.
    ///
    /// `base` is the server's HTTP origin (e.g. `http://127.0.0.1:19132`).
    pub fn connect(base: &str, network_id: &str, identity: &ClientIdentity) -> Result<Session> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        socket.set_nonblocking(false)?;
        let local_addr = socket.local_addr()?;

        let mut pc = RTCPeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .build(Instant::now())
            .map_err(|e| Error::transport(format!("peer connection: {e}")))?;

        // Reliable (ordered) and unreliable (unordered, no retransmits) channels.
        let _ = pc
            .create_data_channel(RELIABLE_CHANNEL, None)
            .map_err(|e| Error::transport(format!("data channel: {e}")))?;
        let unordered = rtc::data_channel::RTCDataChannelInit {
            ordered: false,
            max_retransmits: Some(0),
            ..Default::default()
        };
        let _ = pc
            .create_data_channel(UNRELIABLE_CHANNEL, Some(unordered))
            .map_err(|e| Error::transport(format!("data channel: {e}")))?;

        // Register our UDP host candidate so the server can reach this socket.
        let candidate = CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: local_addr.ip().to_string(),
                port: local_addr.port(),
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()
        .map_err(|e| Error::transport(format!("host candidate: {e}")))?;
        pc.add_local_candidate(
            RTCIceCandidate::from(&candidate)
                .to_json()
                .map_err(|e| Error::transport(format!("candidate json: {e}")))?,
        )
        .map_err(|e| Error::transport(format!("add candidate: {e}")))?;

        let offer = pc
            .create_offer(None)
            .map_err(|e| Error::transport(format!("create offer: {e}")))?;
        pc.set_local_description(Instant::now(), offer.clone())
            .map_err(|e| Error::transport(format!("local description: {e}")))?;

        // Make the offer match the NetherNet shape: no trickle, our host
        // candidate in the reference's format, and end-of-candidates.
        let sdp = pc
            .local_description()
            .map(|d| d.sdp)
            .unwrap_or(offer.sdp);
        let sdp = sdp
            .lines()
            .filter(|l| !l.starts_with("a=ice-options:trickle"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let ufrag = sdp
            .lines()
            .find_map(|l| l.strip_prefix("a=ice-ufrag:"))
            .unwrap_or("")
            .trim()
            .to_string();
        let mut sdp = sdp;
        if !sdp.contains("a=candidate:") {
            let line = format!(
                "a=candidate:1 1 udp 2130706431 {} {} typ host generation 0 ufrag {} network-id 0 network-cost 0\r\n",
                local_addr.ip(),
                local_addr.port(),
                ufrag
            );
            sdp = insert_before_media_end(&sdp, &line);
        }
        let sdp = insert_before_media_end(&sdp, "a=end-of-candidates\r\n");

        let augmented = identity
            .augment(&sdp, SystemTime::now())
            .map_err(|e| Error::auth(format!("identity: {e}")))?;

        let answer_sdp = crate::nethernet::join::join(base, network_id, &augmented)?;
        let answer = RTCSessionDescription::answer(answer_sdp)
            .map_err(|e| Error::transport(format!("parse answer: {e}")))?;
        pc.set_remote_description(Instant::now(), answer)
            .map_err(|e| Error::transport(format!("remote description: {e}")))?;

        Ok(Session {
            pc,
            socket,
            local_addr,
            reliable: None,
            ready: false,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether both the connection and the reliable channel are open.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Drive the connection for at most `budget`, returning any payloads
    /// received on the reliable channel.
    pub fn pump(&mut self, budget: Duration) -> Result<Vec<Vec<u8>>> {
        let deadline = Instant::now() + budget;
        let mut received = Vec::new();
        let mut buf = vec![0u8; 2048];

        while Instant::now() < deadline {
            // Send whatever the backend queued.
            while let Some(msg) = self.pc.poll_write() {
                let _ = self.socket.send_to(&msg.message, msg.transport.peer_addr);
            }

            // Receive a packet, if any.
            match self.socket.recv_from(&mut buf) {
                Ok((n, peer_addr)) => {
                    let tagged = TaggedBytesMut {
                        now: Instant::now(),
                        transport: TransportContext {
                            local_addr: self.local_addr,
                            peer_addr,
                            ecn: None,
                            transport_protocol: TransportProtocol::UDP,
                        },
                        message: BytesMut::from(&buf[..n]),
                    };
                    if self.pc.handle_read(tagged).is_err() {
                        // A malformed/early packet is not fatal; keep going.
                    }
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e.into()),
            }

            // Connection and channel events.
            while let Some(event) = self.pc.poll_event() {
                match event {
                    RTCPeerConnectionEvent::OnConnectionStateChangeEvent(state) => {
                        if state == RTCPeerConnectionState::Failed {
                            return Err(Error::transport("peer connection failed"));
                        }
                        if state == RTCPeerConnectionState::Connected {
                            self.ready = true;
                        }
                    }
                    RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnOpen(id))
                        if self.reliable.is_none() =>
                    {
                        self.reliable = Some(id);
                        self.ready = true;
                    }
                    RTCPeerConnectionEvent::OnDataChannel(RTCDataChannelEvent::OnClose(id))
                        if self.reliable == Some(id) =>
                    {
                        self.reliable = None;
                        self.ready = false;
                    }
                    _ => {}
                }
            }

            // Application messages.
            while let Some(TaggedRTCMessage { message, .. }) = self.pc.poll_read() {
                if let RTCMessage::DataChannelMessage(id, data) = message {
                    if Some(id) == self.reliable {
                        received.push(data.data.to_vec());
                    }
                }
            }

            // Timers.
            if let Some(timeout) = self.pc.poll_timeout() {
                if timeout <= Instant::now() {
                    let _ = self.pc.handle_timeout(Instant::now());
                }
            }

            std::thread::sleep(Duration::from_millis(2));
        }

        Ok(received)
    }

    /// Send a payload on the reliable channel.
    pub fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        let Some(id) = self.reliable else {
            return Err(Error::transport("reliable channel is not open"));
        };
        let mut channel = self
            .pc
            .data_channel(id)
            .ok_or_else(|| Error::transport("reliable channel is gone"))?;
        channel
            .send(Instant::now(), BytesMut::from(data))
            .map_err(|e| Error::transport(format!("send: {e}")))
    }

    pub fn close(&mut self) {
        let _ = self.pc.close();
    }
}

/// Insert `line` just before the final media-level line ends. SDP is
/// line-oriented; appending before the trailing empty line keeps it in the
/// media section.
fn insert_before_media_end(sdp: &str, line: &str) -> String {
    let mut out = String::with_capacity(sdp.len() + line.len() + 2);
    out.push_str(sdp.trim_end_matches(['\r', '\n']));
    if !out.ends_with(['\r', '\n']) {
        out.push_str("\r\n");
    }
    out.push_str(line);
    out
}
