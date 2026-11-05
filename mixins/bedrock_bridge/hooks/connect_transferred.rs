// Body of Session::connect_transferred: a transfer keeps the original path
// (cookies carried); a Bedrock transfer is not supported yet.
fn connect_transferred(
    address: &str,
    name: &str,
    auth: Option<::std::sync::Arc<crate::auth::AuthSession>>,
    info: horizon_protocol::packets::configuration::ClientInformation,
    lang: ::std::sync::Arc<horizon_protocol::text::Language>,
    cookies: crate::net::Cookies,
    resources: ::std::path::PathBuf,
) -> crate::net::Session {
    if crate::net::bedrock::is_bedrock(address) {
        eprintln!("[bedrock] transfers to a Bedrock address are not supported yet");
    }
    crate::net::Session::start(address, name, auth, info, lang, None, Some(cookies), resources, false)
}
