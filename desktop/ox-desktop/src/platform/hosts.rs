pub(crate) use ox_host_web::paired_hosts::{
    Folders, KeyReview, SavedHost, device_label, list, pair,
};
pub(crate) use ox_host_web::{
    client::OpenHost,
    discovery::open_hosts,
    paired_hosts::{pair_window, window_address},
};

/// Hosts on this tailnet waiting for a device. This machine's own Serve port is
/// tried first, because a tailnet usually shares one.
pub(crate) fn waiting_hosts() -> Vec<OpenHost> {
    let port = ox_host_service::Paths::discover().ok().and_then(|paths| {
        ox_host_web::admin::request(&paths.state_dir, &ox_host_web::admin::Request::Status)
            .ok()
            .and_then(|reply| reply.https_port)
    });
    open_hosts(port)
}
