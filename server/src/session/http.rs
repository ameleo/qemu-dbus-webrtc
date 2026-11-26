// SPDX-License-Identifier: MPL-2.0

//! The HTTPS server: the viewer page, and the WebSocket upgrade of its signalling.

use std::net::SocketAddr;

use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tracing::{info, warn};

use std::sync::Arc;

use super::signalling::{Admission, Socket, open};
use crate::config::Auth;
use crate::vms::Vms;

/// A certificate and its private key, PEM files.
#[derive(Debug, Clone)]
pub struct Tls {
    pub certificate: PathBuf,
    pub key: PathBuf,
}

/// Serve the VMs' pages and their signalling on `listen` until the program ends, in HTTPS
/// with `tls`: never in plain HTTP, which would carry the session in clear (and browsers give
/// the clipboard, the microphone and the camera to pages served in HTTPS only). A VM's page is
/// `/vm/<uuid>/`, its signalling `/vm/<uuid>/signal` (`qdw_protocol::signalling_path`).
pub async fn serve(
    listen: SocketAddr,
    tls: axum_server::tls_rustls::RustlsConfig,
    vms: Arc<Vms>,
    auth: Auth,
) -> Result<()> {
    let state = AppState {
        vms,
        auth,
        checks: Arc::new(tokio::sync::Semaphore::new(CHECKS_AT_ONCE)),
    };
    let app =
        axum::Router::new()
            .route(
                "/vm/{uuid}",
                // The path as it came, still percent-encoded: decoded, an encoded '/' in the
                // id would make another path.
                get(|uri: axum::http::Uri| async move {
                    Redirect::permanent(&format!("{}/", uri.path()))
                }),
            )
            .route("/vm/{uuid}/", get(|| page("index.html")))
            .route("/vm/{uuid}/app.js", get(|| page("app.js")))
            .route("/vm/{uuid}/app.css", get(|| page("app.css")))
            .route("/vm/{uuid}/signal", get(upgrade))
            .with_state(state)
            .into_make_service_with_connect_info::<SocketAddr>();
    info!(%listen, "serving the viewer page in HTTPS");
    axum_server::bind_rustls(listen, tls).serve(app).await?;
    Ok(())
}

/// The pages' TLS, from `tls`'s files: reloaded in place when they change (`reload`).
pub async fn load_tls(tls: &Tls) -> Result<axum_server::tls_rustls::RustlsConfig> {
    // One crypto provider for the process: ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.certificate, &tls.key)
        .await
        .with_context(|| {
            format!(
                "loading {} and {}",
                tls.certificate.display(),
                tls.key.display()
            )
        })
}

/// A file of a VM's page, by its UUID or its name. The same page whatever the VM, whether it exists or
/// not: which VMs the host runs is said only to a client that logged in (on the signalling).
/// With the headers that keep it from being framed by another site (its keys go to the guest),
/// and that let it run its own script and style only, and talk to this server only.
async fn page(path: &'static str) -> impl IntoResponse {
    use axum::http::header;
    let file = qdw_web::file(path).expect("a file of the page");
    (
        [
            (header::CONTENT_TYPE, file.content_type),
            (header::CONTENT_SECURITY_POLICY, qdw_web::POLICY),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        file.content,
    )
}

/// Whether a WebSocket upgrade comes from this server's own page, or from no page: a browser
/// says the page's origin, which must be this server's; a native client says none. Another
/// site's page, in the user's browser, must not reach the VMs (cross-site WebSocket hijacking).
fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok());
    match (origin.to_str().ok(), host) {
        (Some(origin), Some(host)) => origin
            .strip_prefix("https://")
            .is_some_and(|o| o.eq_ignore_ascii_case(host)),
        _ => false,
    }
}

/// What the pages and the signalling share.
#[derive(Clone)]
struct AppState {
    vms: Arc<Vms>,
    auth: Auth,
    /// Password checks at once: each takes 64 MiB and a CPU for a while (Argon2), so that many
    /// logins at once, an attacker's, wait their turn rather than exhaust the host.
    checks: Arc<tokio::sync::Semaphore>,
}

/// How many password checks run at once.
const CHECKS_AT_ONCE: usize = 2;

/// The longest VM id taken: a UUID, or a VM's name.
const MAX_ID: usize = 255;
async fn upgrade(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    State(AppState { vms, auth, checks }): State<AppState>,
) -> Response {
    if !same_origin(&headers) {
        warn!(%peer, origin = ?headers.get(axum::http::header::ORIGIN), "WebSocket from another site refused");
        return (StatusCode::FORBIDDEN, "not from this server's page").into_response();
    }
    // Which VM is asked only once the client logged in: before, the answer would tell anyone
    // which VMs the host runs. The id must look like one, a UUID or a name.
    if id.is_empty() || id.len() > MAX_ID || id.contains(['/', '\\', '\0']) {
        return (StatusCode::NOT_FOUND, "no such VM on this host").into_response();
    }
    let stopping = vms.stopping();
    ws.on_upgrade(move |socket| async move {
        info!(%peer, id, "viewer connected");
        let admission = Admission::Local {
            auth: &auth,
            checks: &checks,
        };
        if let Err(e) = open(Socket::Ws(socket), &vms, admission, &id, stopping).await {
            warn!(%peer, id, error = %format!("{e:#}"), "viewer session ended");
        }
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websockets_from_this_server_or_no_page() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let headers = |origin: Option<&str>| {
            let mut h = HeaderMap::new();
            h.insert(
                header::HOST,
                HeaderValue::from_static("vdi.example.com:8090"),
            );
            if let Some(origin) = origin {
                h.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            h
        };
        assert!(same_origin(&headers(None)));
        assert!(same_origin(&headers(Some("https://vdi.example.com:8090"))));
        assert!(same_origin(&headers(Some("https://VDI.example.com:8090"))));
        assert!(!same_origin(&headers(Some("https://evil.example.org"))));
        assert!(!same_origin(&headers(Some("http://vdi.example.com:8090"))));
        assert!(!same_origin(&headers(Some("https://vdi.example.com:8091"))));
        assert!(!same_origin(&headers(Some("null"))));
    }
}
