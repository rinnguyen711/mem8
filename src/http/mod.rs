//! Serving mem8 over HTTP, for running it as a shared service.
//!
//! Off by default and behind the `http` feature. The ordinary shape is stdio:
//! the agent spawns mem8 as a child process, and the memory belongs to one user
//! on one machine. This module exists for the other shape — one mem8, several
//! clients, reached over a network — which needs authentication, transport
//! security, and an explicit project scope, none of which stdio needs.

pub mod auth;

#[cfg(feature = "http")]
use crate::core::{Memory8, ScopeMode};
#[cfg(feature = "http")]
use crate::mcp::Mem8Server;
#[cfg(feature = "http")]
use auth::Token;
use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(feature = "http")]
use std::sync::Arc;

/// How the listener is secured.
pub enum Tls {
    /// A certificate and key, in PEM form.
    Enabled { cert: PathBuf, key: PathBuf },
    /// No TLS. Only permitted for loopback binds, or with an explicit override.
    Disabled { insecure_override: bool },
}

/// Whether an address is reachable only from this machine.
///
/// A loopback bind cannot be reached from the network, so plaintext there is
/// the reverse-proxy shape: the proxy holds the certificate and mem8 never
/// emits plaintext beyond the host.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Refuse to serve a bearer token over plaintext to the network.
///
/// A token in an `Authorization` header on an unencrypted connection is
/// readable by every device on the path, and a captured token is complete
/// access to every memory. Refusing to start is what makes this hard to get
/// wrong: a warning printed into a container log would scroll past unread.
///
/// Compiled without the `http` feature too, so its tests run in every build —
/// a security guard that is only checked in one configuration is one that can
/// silently rot in the others.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn check_transport_security(addr: &SocketAddr, tls: &Tls) -> Result<(), String> {
    match tls {
        Tls::Enabled { .. } => Ok(()),
        Tls::Disabled { insecure_override } => {
            if is_loopback(addr) {
                // Not reachable off-host; the proxy in front terminates TLS.
                return Ok(());
            }
            if *insecure_override {
                eprintln!(
                    "mem8: WARNING - serving {addr} without TLS because --insecure was given. \
                     The bearer token is sent in plaintext and can be captured by anything on \
                     the network path."
                );
                return Ok(());
            }
            Err(format!(
                "refusing to bind {addr} without TLS.\n\
                 A bearer token sent over plaintext HTTP is readable by anything on the network \
                 path, and a captured token grants full access to every memory.\n\
                 Either:\n  \
                 - pass --tls-cert and --tls-key, or\n  \
                 - bind 127.0.0.1 and terminate TLS in a reverse proxy, or\n  \
                 - pass --insecure if this is a trusted private network."
            ))
        }
    }
}

/// The router, exposed so integration tests can bind it to an ephemeral port.
///
/// Tests must exercise the real router rather than a re-created approximation:
/// a test that assembled its own middleware stack would keep passing after the
/// production one lost its auth layer.
#[cfg(feature = "http")]
pub fn router_for_tests(service: Arc<Memory8>, token: Token) -> axum::Router {
    router(service, token)
}

/// Build the axum router: the MCP endpoint, behind the auth middleware.
#[cfg(feature = "http")]
fn router(service: Arc<Memory8>, token: Token) -> axum::Router {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let mcp = StreamableHttpService::new(
        move || Ok(Mem8Server::new(service.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    axum::Router::new()
        // Liveness. Deliberately *inside* the auth layer: an unauthenticated
        // endpoint would let anyone confirm that a mem8 server is listening
        // here, which is the first step of attacking it. A container health
        // check can carry the token like any other client.
        .route("/health", axum::routing::get(|| async { "ok" }))
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn_with_state(
            token,
            auth::require_token,
        ))
}

/// Serve MCP over HTTP until the process is stopped.
///
/// The service is built in `Explicit` scope mode: a remote caller must name its
/// project, because the server's working directory describes the server rather
/// than the caller.
#[cfg(feature = "http")]
pub async fn serve_http(addr: SocketAddr, tls: Tls, token: Token) -> anyhow::Result<()> {
    check_transport_security(&addr, &tls).map_err(|e| anyhow::anyhow!(e))?;

    let service = Arc::new(
        crate::cli::build_service()
            .await?
            .with_scope_mode(ScopeMode::Explicit),
    );

    let app = router(service, token);

    match tls {
        Tls::Enabled { cert, key } => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "could not load the TLS certificate from {} and {}: {e}",
                        cert.display(),
                        key.display()
                    )
                })?;

            eprintln!("mem8: serving MCP over HTTPS on {addr}");
            axum_server::bind_rustls(addr, config)
                .serve(app.into_make_service())
                .await?;
        }
        Tls::Disabled { .. } => {
            eprintln!("mem8: serving MCP over HTTP on {addr}");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn no_tls() -> Tls {
        Tls::Disabled {
            insecure_override: false,
        }
    }

    fn with_tls() -> Tls {
        Tls::Enabled {
            cert: "cert.pem".into(),
            key: "key.pem".into(),
        }
    }

    #[test]
    fn a_public_bind_without_tls_is_refused() {
        let err = check_transport_security(&addr("0.0.0.0:8080"), &no_tls()).unwrap_err();
        assert!(err.contains("TLS"), "the error must explain why: {err}");
        assert!(
            err.contains("--tls-cert"),
            "the error must say how to fix it: {err}"
        );
    }

    #[test]
    fn a_routable_address_without_tls_is_refused() {
        assert!(check_transport_security(&addr("192.168.1.10:8080"), &no_tls()).is_err());
    }

    #[test]
    fn loopback_without_tls_is_allowed() {
        // The reverse-proxy shape: nothing off-host can reach this socket.
        assert!(check_transport_security(&addr("127.0.0.1:8080"), &no_tls()).is_ok());
        assert!(check_transport_security(&addr("[::1]:8080"), &no_tls()).is_ok());
    }

    #[test]
    fn a_public_bind_with_tls_is_allowed() {
        assert!(check_transport_security(&addr("0.0.0.0:8080"), &with_tls()).is_ok());
    }

    #[test]
    fn the_insecure_override_permits_a_public_plaintext_bind() {
        // Deliberately possible, deliberately requiring a typed flag.
        let tls = Tls::Disabled {
            insecure_override: true,
        };
        assert!(check_transport_security(&addr("0.0.0.0:8080"), &tls).is_ok());
    }
}
