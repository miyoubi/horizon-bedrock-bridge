// Cancellable HEAD of Session::connect_transferred: a transfer to a Bedrock
// address is not supported yet, so a Bedrock target is refused by letting the
// original path run (which will simply fail to connect).
fn connect_transferred(
    address: &str,
    name: &str,
    auth: Option<::std::sync::Arc<crate::auth::AuthSession>>,
    view_distance: i8,
    lang: ::std::sync::Arc<horizon_protocol::text::Language>,
    cookies: crate::net::Cookies,
    resources: ::std::path::PathBuf,
) -> ::core::ops::ControlFlow<crate::net::Session> {
    let _ = (name, auth, view_distance, lang, cookies, resources);
    if crate::net::bedrock::is_bedrock(address) {
        eprintln!("[bedrock] transfers to a Bedrock address are not supported yet");
    }
    ::core::ops::ControlFlow::Continue(())
}
