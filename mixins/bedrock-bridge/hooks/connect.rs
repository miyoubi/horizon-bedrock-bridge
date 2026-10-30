// Cancellable HEAD of Session::connect: for a `bedrock:` address the bridge
// builds the Session and Break returns it, skipping the Java connection path;
// `Continue(())` leaves every other address to the original body.
fn connect(
    address: &str,
    name: &str,
    auth: Option<::std::sync::Arc<crate::auth::AuthSession>>,
    view_distance: i8,
    lang: ::std::sync::Arc<horizon_protocol::text::Language>,
    resources: ::std::path::PathBuf,
) -> ::core::ops::ControlFlow<crate::net::Session> {
    let _ = (auth, lang);
    match crate::net::bedrock::try_connect(address, name, view_distance, resources) {
        Some(session) => ::core::ops::ControlFlow::Break(session),
        None => ::core::ops::ControlFlow::Continue(()),
    }
}
