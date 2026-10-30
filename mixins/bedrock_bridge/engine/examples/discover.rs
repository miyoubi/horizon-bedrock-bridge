//! LAN discovery probe: send a NetherNet discovery request and print the reply.
//!
//!   cargo run -p bedrock-core --example discover -- 127.0.0.1:7551
//!
//! Works against a local NetherNet server (UDP 7551); no WebRTC needed.

use std::net::UdpSocket;
use std::time::Duration;

use bedrock_core::nethernet::discovery;

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:7551".into());
    let socket = UdpSocket::bind("0.0.0.0:0").expect("bind udp");
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let request = discovery::encode_request(0x1122_3344_5566_7788);
    socket
        .send_to(&request, &addr)
        .unwrap_or_else(|e| panic!("send to {addr}: {e}"));

    let mut buf = [0u8; 65535];
    match socket.recv_from(&mut buf) {
        Ok((n, from)) => match discovery::decode_response(&buf[..n]) {
            Ok((data, sender)) => {
                println!("reply from {from} (sender {sender:#018x})");
                println!(
                    "name={:?} level={:?} players={}/{} protocol={} version={:?} gameType={} session={:?}",
                    data.server_name,
                    data.level_name,
                    data.player_count,
                    data.max_player_count,
                    data.protocol_version,
                    data.game_version,
                    data.game_type,
                    data.session_id
                );
            }
            Err(e) => println!("decode error: {e} ({} bytes)", n),
        },
        Err(e) => println!("recv error: {e}"),
    }
}
