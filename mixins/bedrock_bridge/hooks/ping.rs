// HEAD of `net::ping`: the server list pings with the Java status protocol,
// which a Bedrock server does not speak, so every Bedrock entry read as
// unreachable. A Bedrock address answers with a synthesized status instead.
fn ping(
    address: &str,
    timeout: ::std::time::Duration,
) -> ::std::ops::ControlFlow<Result<(serde_json::Value, u128), String>> {
    let _ = timeout;
    match crate::net::bedrock::status(address) {
        Some(status) => ::std::ops::ControlFlow::Break(Ok((status, 0))),
        None => ::std::ops::ControlFlow::Continue(()),
    }
}
