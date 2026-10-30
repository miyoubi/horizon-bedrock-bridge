// Body of Session::connect: a Bedrock address becomes the bridge's Session,
// every other address keeps the original path (Session::start with no pipe).
fn connect(
    address: &str,
    name: &str,
    auth: Option<::std::sync::Arc<crate::auth::AuthSession>>,
    view_distance: i8,
    lang: ::std::sync::Arc<horizon_protocol::text::Language>,
    resources: ::std::path::PathBuf,
) -> crate::net::Session {
    if let Some(session) =
        crate::net::bedrock::try_connect(address, name, view_distance, resources.clone())
    {
        return session;
    }
    crate::net::Session::start(
        address,
        name,
        auth,
        view_distance,
        lang,
        None,
        None,
        resources,
    )
}
