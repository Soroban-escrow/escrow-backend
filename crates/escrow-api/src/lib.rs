#![forbid(unsafe_code)]
//! The HTTP API (axum). Thin by design: it authenticates, validates, reads the database, asks the chain to
//! prepare transactions for a user's wallet, and enqueues webhook work. It never changes a bounty's status
//! (the indexer does) and never holds a user's key.
//!
//! Every response is JSON; errors are `{ "error": { "code", "message" } }`. See `docs/API.md`.

pub mod error;
pub mod extract;
pub mod limiter;
pub mod login;
pub mod metrics;
pub mod routes;
pub mod session;
pub mod state;
pub mod wallet;

use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request},
    http::{HeaderName, HeaderValue, Method, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use error::ApiError;
use extract::ClientIp;
use metrics::Metrics;
use routes::{admin, auth, funding, public, webhook};
pub use state::{ApiConfig, AppState};
use std::{net::SocketAddr, time::Instant};
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer};

const GLOBAL_PER_MINUTE: u32 = 300;
const BODY_LIMIT: usize = 1 << 20;
const WEBHOOK_BODY_LIMIT: usize = 10 << 20;

/// The client address in an `X-Forwarded-For` value. Proxies append on the right, so with `hops`
/// trusted proxies in front the client is the entry `hops` from the right; everything further left
/// is whatever the client sent and is ignored. `None` when the header is too short to trust.
fn xff_client_ip(xff: &str, hops: usize) -> Option<String> {
    if hops == 0 {
        return None;
    }
    let parts: Vec<&str> = xff.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let ip = parts.get(parts.len().checked_sub(hops)?)?;
    (ip.len() <= 45).then(|| (*ip).to_string())
}

fn client_ip(req: &Request, trust_proxy: bool, proxy_hops: usize) -> String {
    if trust_proxy
        && let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(ip) = xff_client_ip(xff, proxy_hops)
    {
        return ip;
    }
    req.extensions().get::<ConnectInfo<SocketAddr>>().map_or_else(|| "unknown".into(), |c| c.0.ip().to_string())
}

/// Resolves the caller's address, counts the request, and applies the per-address ceiling (health checks are exempt).
async fn edge(axum::extract::State(s): axum::extract::State<AppState>, mut req: Request, next: Next) -> Response {
    let ip = client_ip(&req, s.cfg.trust_proxy, s.cfg.proxy_hops as usize);
    Metrics::inc(&s.metrics.http_requests);
    if !matches!(req.uri().path(), "/health" | "/livez")
        && !limiter::check_shared(&s.db.pool, &s.limiter, &format!("g:{ip}"), GLOBAL_PER_MINUTE, Instant::now()).await
    {
        Metrics::inc(&s.metrics.http_rate_limited);
        return ApiError::rate_limited().into_response();
    }
    req.extensions_mut().insert(ClientIp(ip));
    let resp = next.run(req).await;
    if resp.status().is_server_error() {
        Metrics::inc(&s.metrics.http_server_errors);
    }
    resp
}

pub fn router(state: AppState) -> Router {
    let secure = state.cfg.secure_cookies;
    let cors = CorsLayer::new()
        .allow_origin(HeaderValue::from_str(&state.cfg.web_public_url).expect("WEB_PUBLIC_URL is a valid header value"))
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-csrf-token")])
        .max_age(std::time::Duration::from_secs(600));

    let mut app = Router::new()
        .route("/health", get(public::health))
        .route("/livez", get(public::livez))
        .route("/metrics", get(public::metrics))
        .route("/config", get(public::config))
        .route("/stats", get(public::stats))
        .route("/bounties", get(public::list_bounties))
        .route("/bounties/{id}", get(public::get_bounty))
        .route("/users/{login}", get(public::get_user))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/wallet/challenge", post(auth::wallet_challenge))
        .route("/wallet/verify", post(auth::wallet_verify))
        .route("/wallet", delete(auth::wallet_unlink))
        .route("/me/earnings", get(auth::my_earnings))
        .route("/me/funded", get(auth::my_funded))
        .route("/intents/{id}", get(funding::get_intent))
        .route("/intents/{id}/build", post(funding::build_fund))
        .route("/tx/submit", post(funding::submit))
        .route("/bounties/{id}/release/build", post(funding::build_release))
        .route("/bounties/{id}/refund/build", post(funding::build_refund))
        .route("/admin/disputes", get(admin::list_disputes))
        .route("/admin/disputes/{id}/open-onchain", post(admin::open_onchain))
        .route("/admin/disputes/{id}/resolve/build", post(admin::resolve_build))
        .route("/admin/disputes/{id}/reject", post(admin::reject))
        .route("/admin/audit", get(admin::list_audit))
        .route("/admin/payouts", get(admin::list_payouts))
        .route("/admin/payouts/{id}/retry", post(admin::retry_payout))
        .route("/admin/status", get(admin::status))
        .route("/admin/repos/{id}/ai-reviews", put(admin::set_ai_reviews))
        .route("/github/webhooks", post(webhook::receive).layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT)))
        .fallback(|| async { ApiError::not_found("route") })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(state.clone(), edge))
        .layer(cors)
        .layer(SetResponseHeaderLayer::if_not_present(header::CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")))
        .layer(SetResponseHeaderLayer::overriding(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ))
        // spans carry the path only: query strings hold one-time OAuth codes
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|r: &axum::http::Request<_>| tracing::info_span!("http", method = %r.method(), path = %r.uri().path())),
        );
    if secure {
        app = app.layer(SetResponseHeaderLayer::overriding(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=63072000; includeSubDomains"),
        ));
    }
    app.with_state(state)
}

/// Serves until `shutdown` resolves, finishing in-flight requests first.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await
}

#[cfg(test)]
mod tests {
    use super::xff_client_ip;

    #[test]
    fn the_client_is_counted_from_the_right_past_the_trusted_proxies() {
        // one trusted proxy: the right-most entry is what it observed; the spoofed left-most is ignored
        assert_eq!(xff_client_ip("1.2.3.4, 203.0.113.9", 1).as_deref(), Some("203.0.113.9"));
        // two trusted proxies: skip the inner proxy's entry too
        assert_eq!(xff_client_ip("9.9.9.9, 203.0.113.9, 10.0.0.1", 2).as_deref(), Some("203.0.113.9"));
        // a header shorter than the trusted chain cannot be trusted
        assert_eq!(xff_client_ip("203.0.113.9", 2), None);
        assert_eq!(xff_client_ip("", 1), None);
        assert_eq!(xff_client_ip("anything", 0), None);
    }
}
