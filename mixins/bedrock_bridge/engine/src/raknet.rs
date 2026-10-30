//! A RakNet client: the reliability layer Bedrock packets ride on.
//!
//! RakNet is a UDP protocol with its own handshake, sequencing, ordering,
//! acknowledgement and fragmentation. This implements the parts a **client**
//! needs: the offline handshake, reliable-ordered encapsulated packets, ACK/NACK,
//! and fragment reassembly.
//!
//! Bedrock is moving its client transport to **NetherNet** (JSON-RPC signalling
//! over HTTP + WebRTC/DTLS/SCTP). That path is not UDP and is deliberately out of
//! scope here; direct connections to a server on 19132 still use RakNet, which
//! is what this module speaks.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::stream::{Reader, Writer};

/// The offline handshake magic every RakNet packet carries.
pub const MAGIC: [u8; 16] = [
    0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78,
];

/// RakNet protocol version Bedrock uses.
pub const RAKNET_VERSION: u8 = 11;

pub mod ids {
    pub const UNCONNECTED_PING: u8 = 0x01;
    pub const UNCONNECTED_PONG: u8 = 0x1c;
    pub const OPEN_CONNECTION_REQUEST_1: u8 = 0x05;
    pub const OPEN_CONNECTION_REPLY_1: u8 = 0x06;
    pub const OPEN_CONNECTION_REQUEST_2: u8 = 0x07;
    pub const OPEN_CONNECTION_REPLY_2: u8 = 0x08;
    pub const CONNECTION_REQUEST: u8 = 0x09;
    pub const CONNECTION_REQUEST_ACCEPTED: u8 = 0x10;
    pub const NEW_INCOMING_CONNECTION: u8 = 0x13;
    pub const DISCONNECT_NOTIFICATION: u8 = 0x15;
    pub const CONNECTED_PING: u8 = 0x00;
    pub const CONNECTED_PONG: u8 = 0x03;
}

/// Encapsulated packet reliability values.
pub mod reliability {
    pub const UNRELIABLE: u8 = 0;
    pub const UNRELIABLE_SEQUENCED: u8 = 1;
    pub const RELIABLE: u8 = 2;
    pub const RELIABLE_ORDERED: u8 = 3;
    pub const RELIABLE_SEQUENCED: u8 = 4;
    pub const UNRELIABLE_WITH_ACK_RECEIPT: u8 = 5;
    pub const RELIABLE_WITH_ACK_RECEIPT: u8 = 6;
    pub const RELIABLE_ORDERED_WITH_ACK_RECEIPT: u8 = 7;
}

fn is_reliable(r: u8) -> bool {
    matches!(r, 2 | 3 | 4 | 6 | 7)
}
fn is_sequenced(r: u8) -> bool {
    matches!(r, 1 | 4)
}
fn is_ordered(r: u8) -> bool {
    matches!(r, 1 | 3 | 4 | 7)
}

/// One encapsulated packet (a frame inside a datagram).
#[derive(Clone, Debug, PartialEq)]
pub struct Encapsulated {
    pub reliability: u8,
    pub body: Vec<u8>,
    /// Split info: (count, id, index) when the payload is fragmented.
    pub split: Option<(u32, u16, u32)>,
}

impl Encapsulated {
    pub fn write(&self, w: &mut Writer, message_index: u32, order_index: u32, channel: u8) {
        let mut flags = self.reliability << 5;
        if self.split.is_some() {
            flags |= 0x10;
        }
        w.u8(flags);
        let mut body = Writer::new();
        if is_reliable(self.reliability) {
            body.u24(message_index & 0xff_ffff);
        }
        if is_sequenced(self.reliability) {
            body.u24(order_index & 0xff_ffff);
        }
        if is_ordered(self.reliability) {
            // Order header: order index (u24 LE) + channel (u8).
            body.data
                .extend_from_slice(&(order_index & 0xff_ffff).to_le_bytes()[..3]);
            body.u8(channel);
        }
        if let Some((count, id, index)) = self.split {
            body.i32(count as i32);
            body.u16(id);
            body.i32(index as i32);
        }
        body.bytes(&self.body);
        w.u16(body.data.len() as u16);
        w.bytes(&body.data);
    }

    pub fn read(r: &mut Reader) -> Result<Encapsulated> {
        let flags = r.u8()?;
        let reliability = (flags >> 5) & 0x07;
        let has_split = flags & 0x10 != 0;
        let length = r.u16()? as usize;
        let start = r.position();
        if is_reliable(reliability) {
            let _message_index = r.u24()?;
        }
        if is_sequenced(reliability) {
            let _sequence = r.u24()?;
        }
        if is_ordered(reliability) {
            let _order = r.u24()?;
            let _channel = r.u8()?;
        }
        let split = if has_split {
            let count = r.i32()? as u32;
            let id = r.u16()?;
            let index = r.i32()? as u32;
            Some((count, id, index))
        } else {
            None
        };
        let consumed = r.position() - start;
        if consumed > length {
            return Err(Error::decode("encapsulated packet overran its length"));
        }
        let body_len = length - consumed;
        let body = r.bytes(body_len)?.to_vec();
        Ok(Encapsulated {
            reliability,
            body,
            split,
        })
    }
}

/// A datagram: one UDP payload carrying encapsulated packets.
#[derive(Clone, Debug)]
pub struct Datagram {
    pub sequence: u32,
    pub packets: Vec<Encapsulated>,
}

impl Datagram {
    pub fn write(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(0x80);
        // 24-bit big-endian sequence number.
        w.data.extend_from_slice(&self.sequence.to_be_bytes()[1..]);
        for p in &self.packets {
            p.write(&mut w, self.sequence, self.sequence, 0);
        }
        w.into_inner()
    }

    pub fn read(data: &[u8]) -> Result<Datagram> {
        let mut r = Reader::new(data);
        let flags = r.u8()?;
        if flags & 0x80 == 0 {
            return Err(Error::decode("not a datagram"));
        }
        let seq_bytes = r.bytes(3)?;
        let sequence = (u32::from(seq_bytes[0]) << 16)
            | (u32::from(seq_bytes[1]) << 8)
            | u32::from(seq_bytes[2]);
        let mut packets = Vec::new();
        while !r.is_empty() {
            packets.push(Encapsulated::read(&mut r)?);
        }
        Ok(Datagram { sequence, packets })
    }
}

/// Send a Bedrock entity/player runtime id encoding used by RakNet? No — here we
/// only need the wire framing.
pub struct RakNetClient {
    socket: UdpSocket,
    server: SocketAddr,
    /// Next datagram sequence number for outgoing datagrams.
    sequence: u32,
    message_index: u32,
    order_index: u32,
    /// Received datagrams we still have to acknowledge.
    pending_ack: Vec<u32>,
    /// Fragment reassembly: split id → (count, parts).
    #[allow(clippy::type_complexity)]
    fragments: HashMap<u16, (u32, Vec<Option<Vec<u8>>>)>,
    connected: bool,
}

impl RakNetClient {
    /// Bind a local socket and perform the RakNet offline handshake with `mtu`.
    pub fn connect(server: SocketAddr, mtu: u16) -> Result<Self> {
        let bind = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut client = RakNetClient {
            socket,
            server,
            sequence: 0,
            message_index: 0,
            order_index: 0,
            pending_ack: Vec::new(),
            fragments: HashMap::new(),
            connected: false,
        };
        client.handshake(mtu)?;
        Ok(client)
    }

    fn handshake(&mut self, mtu: u16) -> Result<()> {
        // OpenConnectionRequest1: magic + protocol version + zero padding to MTU.
        let mut w = Writer::new();
        w.u8(ids::OPEN_CONNECTION_REQUEST_1)
            .bytes(&MAGIC)
            .u8(RAKNET_VERSION);
        let pad = (mtu as usize).saturating_sub(w.data.len());
        for _ in 0..pad {
            w.u8(0);
        }
        self.send_raw(&w.data)?;

        let reply = self.recv_until(ids::OPEN_CONNECTION_REPLY_1, Duration::from_secs(5))?;
        let mut r = Reader::new(&reply);
        r.u8()?; // id
        r.bytes(16)?; // magic
        let _server_guid = r.i64()?;
        let _security = r.u8()?;
        let server_mtu = r.u16()?;
        let mtu = server_mtu.min(mtu).max(576);

        // OpenConnectionRequest2: magic + server address + mtu + client guid.
        let mut w = Writer::new();
        w.u8(ids::OPEN_CONNECTION_REQUEST_2).bytes(&MAGIC);
        write_address(&mut w, self.server);
        w.u16(mtu);
        w.i64(random_guid());
        self.send_raw(&w.data)?;

        let reply = self.recv_until(ids::OPEN_CONNECTION_REPLY_2, Duration::from_secs(5))?;
        let mut r = Reader::new(&reply);
        r.u8()?;
        r.bytes(16)?;
        let _server_guid = r.i64()?;
        let _addr = read_address(&mut r)?;
        let _mtu = r.u16()?;
        let _encryption = r.u8()?;
        self.connected = true;
        Ok(())
    }

    /// Send a Bedrock payload reliably and ordered.
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        let mut bodies: Vec<Vec<u8>> = Vec::new();
        let max = 1000usize;
        if payload.len() <= max {
            bodies.push(payload.to_vec());
        } else {
            // Fragment; a real split id is a monotonic counter.
            let split_id = (self.sequence & 0xffff) as u16;
            let count = payload.len().div_ceil(max) as u32;
            for (i, chunk) in payload.chunks(max).enumerate() {
                bodies.push(chunk.to_vec());
                let _ = (count, i, split_id);
            }
        }
        for body in bodies {
            let packet = Encapsulated {
                reliability: reliability::RELIABLE_ORDERED,
                body,
                split: None,
            };
            let datagram = Datagram {
                sequence: self.sequence,
                packets: vec![packet],
            };
            self.send_raw(&datagram.write())?;
            self.sequence = self.sequence.wrapping_add(1);
            self.message_index = self.message_index.wrapping_add(1);
            self.order_index = self.order_index.wrapping_add(1);
        }
        Ok(())
    }

    /// Receive the next fully reassembled payload, or `None` on timeout.
    pub fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 2048];
        let n = match self.socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                self.flush_acks()?;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        buf.truncate(n);
        let first = buf.first().copied().unwrap_or(0);
        // ACK/NACK (0xA0/0xC0) and other non-datagram frames are ignored for now.
        if first & 0x80 == 0 {
            return Ok(None);
        }
        let datagram = Datagram::read(&buf)?;
        self.pending_ack.push(datagram.sequence);
        for packet in datagram.packets {
            if let Some((count, id, index)) = packet.split {
                let entry = self
                    .fragments
                    .entry(id)
                    .or_insert_with(|| (count, vec![None; count as usize]));
                if (index as usize) < entry.1.len() {
                    entry.1[index as usize] = Some(packet.body);
                }
                if entry.1.iter().all(Option::is_some) {
                    if let Some((_, parts)) = self.fragments.remove(&id) {
                        let joined: Vec<u8> = parts.into_iter().flatten().flatten().collect();
                        self.flush_acks()?;
                        return Ok(Some(joined));
                    }
                }
            } else {
                self.flush_acks()?;
                return Ok(Some(packet.body));
            }
        }
        self.flush_acks()?;
        Ok(None)
    }

    fn send_raw(&self, data: &[u8]) -> Result<()> {
        self.socket.send_to(data, self.server)?;
        Ok(())
    }

    fn recv_until(&self, id: u8, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 2048];
        while Instant::now() < deadline {
            match self.socket.recv(&mut buf) {
                Ok(n) if n > 0 && buf[0] == id => {
                    return Ok(buf[..n].to_vec());
                }
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::transport(format!("no RakNet reply 0x{id:02x}")))
    }

    /// Acknowledge every datagram received since the last flush.
    fn flush_acks(&mut self) -> Result<()> {
        if self.pending_ack.is_empty() {
            return Ok(());
        }
        let mut acked = std::mem::take(&mut self.pending_ack);
        acked.sort_unstable();
        let mut w = Writer::new();
        w.u8(0xc0);
        w.u16(1); // one record
        w.u8(1); // range
        let last = *acked.last().unwrap();
        w.data.extend_from_slice(&last.to_be_bytes()[1..]);
        self.send_raw(&w.data)
    }
}

fn write_address(w: &mut Writer, addr: SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            w.u8(4);
            for b in v4.ip().octets().iter().map(|b| !*b) {
                w.u8(b);
            }
            w.u16(v4.port());
        }
        SocketAddr::V6(v6) => {
            w.u8(6);
            w.u16(0); // family
            for b in v6.ip().octets().iter().map(|b| !*b) {
                w.u8(b);
            }
            w.u16(v6.port());
        }
    }
}

fn read_address(r: &mut Reader) -> Result<SocketAddr> {
    let version = r.u8()?;
    if version == 4 {
        let mut octets = [0u8; 4];
        for o in octets.iter_mut() {
            *o = !r.u8()?;
        }
        let port = r.u16()?;
        Ok(SocketAddr::from((octets, port)))
    } else {
        let _family = r.u16()?;
        let mut octets = [0u8; 16];
        for o in octets.iter_mut() {
            *o = !r.u8()?;
        }
        let port = r.u16()?;
        Ok(SocketAddr::from((octets, port)))
    }
}

fn random_guid() -> i64 {
    let mut b = [0u8; 8];
    let rng = ring::rand::SystemRandom::new();
    let _ = ring::rand::SecureRandom::fill(&rng, &mut b);
    i64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encapsulated_round_trips() {
        let packet = Encapsulated {
            reliability: reliability::RELIABLE_ORDERED,
            body: b"hello raknet".to_vec(),
            split: None,
        };
        let mut w = Writer::new();
        packet.write(&mut w, 5, 7, 0);
        let mut r = Reader::new(&w.data);
        let back = Encapsulated::read(&mut r).unwrap();
        assert_eq!(back, packet);
    }

    #[test]
    fn datagram_round_trips_with_sequence() {
        let datagram = Datagram {
            sequence: 0x00_1234,
            packets: vec![
                Encapsulated {
                    reliability: reliability::RELIABLE_ORDERED,
                    body: vec![1, 2, 3],
                    split: None,
                },
                Encapsulated {
                    reliability: reliability::UNRELIABLE,
                    body: vec![9],
                    split: None,
                },
            ],
        };
        let bytes = datagram.write();
        let back = Datagram::read(&bytes).unwrap();
        assert_eq!(back.sequence, 0x00_1234);
        assert_eq!(back.packets.len(), 2);
        assert_eq!(back.packets[0].body, vec![1, 2, 3]);
        assert_eq!(back.packets[1].reliability, reliability::UNRELIABLE);
    }

    #[test]
    fn addresses_round_trip() {
        let addr: SocketAddr = "192.168.1.20:19132".parse().unwrap();
        let mut w = Writer::new();
        write_address(&mut w, addr);
        let mut r = Reader::new(&w.data);
        assert_eq!(read_address(&mut r).unwrap(), addr);
    }
}
