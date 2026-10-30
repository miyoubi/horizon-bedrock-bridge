// HEAD of Session::disconnect: stop the Bedrock thread by dropping its bridge.
fn disconnect(session: &mut crate::net::Session) {
    crate::net::bedrock::on_disconnect(&session.address);
}
