// Cancellable HEAD of Session::send: when this Session is the Bedrock bridge's,
// the packet is translated and handed to the client thread, and Break(()) skips
// the Java encoder. Java sessions Continue(()) into the original body.
fn send(
    session: &crate::net::Session,
    packet: &horizon_protocol::packets::play::Serverbound,
) -> ::core::ops::ControlFlow<()> {
    if crate::net::bedrock::send(session, packet) {
        ::core::ops::ControlFlow::Break(())
    } else {
        ::core::ops::ControlFlow::Continue(())
    }
}
