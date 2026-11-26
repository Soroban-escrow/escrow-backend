#![forbid(unsafe_code)]
//! The HTTP API (axum). Thin by design: it authenticates, validates, reads the database, asks the chain to
//! prepare transactions for a user's wallet, and enqueues webhook work. It never changes a bounty's status
//! (the indexer does) and never holds a user's key.
//!
//! Every response is JSON; errors are `{ "error": { "code", "message", "request_id" } }`, and every
//! response carries `x-request-id`. See `docs/API.md`.

pub mod error;
pub mod extract;
pub mod limiter;
pub mod login;
pub mod metrics;
mod request_id;
pub mod routes;
pub mod session;
pub mod state;
pub mod wallet;

use axum::{
    Router,
    extract::{ConnectInfo, DefaultBodyLimit, Request},
    http::{HeaderName, HeaderValue, Method, header},
    middleware::{Next, from_fn, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use error::ApiError;
use extract::ClientIp;
use metrics::Metrics;
use request_id::middleware as request_id_middleware;
use routes::{admin, auth, funding, public, webhook};
pub use state::{ApiConfig, AppState};
use std::{net::SocketAddr, time::Instant};
use tower_http::{cors::CorsLayer, request_id::RequestId, set_header::SetResponseHeaderLayer, trace::TraceLayer};

const GLOBAL_PER_MINUTE: u32 = 300;
const BODY_LIMIT: usize = 1 << 20;
const WEBHOOK_BODY_LIMIT: usize = 10 << 20;

fn client_ip(req: &Request, trust_proxy: bool, trust_proxy_hops: usize) -> String {
    if trust_proxy
        && let Some(idx) = trust_proxy_hops.checked_sub(1)
        && let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').map(str::trim).filter(|s| !s.is_empty()).nth(idx))
        && ip.len() <= 45
    {
        return ip.to_string();
    }
    req.extensions().get::<ConnectInfo<SocketAddr>>().map_or_else(|| "unknown".into(), |c| c.0.ip().to_string())
}

/// Resolves the caller's address, counts the request, and applies the per-address ceiling (health checks are exempt).
async fn edge(axum::extract::State(s): axum::extract::State<AppState>, mut req: Request, next: Next) -> Response {
    let ip = client_ip(&req, s.cfg.trust_proxy, s.cfg.trust_proxy_hops);
    Metrics::inc(&s.metrics.http_requests);
    if req.uri().path() != "/health"
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
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-csrf-token"), HeaderName::from_static(request_id::HEADER)])
        .max_age(std::time::Duration::from_secs(600));

    let mut app = Router::new()
        .route("/health", get(public::health))
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
        .route("/admin/payouts", get(admin::list_payouts))
        .route("/admin/payouts/{id}/retry", post(admin::retry_payout))
        .route("/admin/payouts/{id}/reconcile", post(admin::reconcile_payout))
        .route("/admin/status", get(admin::status))
        .route("/admin/audit", get(admin::list_audit))
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
        // spans carry the path only: query strings hold one-time OAuth codes. The request id is set by
        // the outermost layer, so it is already in the extensions when this span is made.
        .layer(TraceLayer::new_for_http().make_span_with(|r: &axum::http::Request<_>| {
            let request_id = r.extensions().get::<RequestId>().and_then(|id| id.header_value().to_str().ok()).unwrap_or("unknown");
            tracing::info_span!("http", method = %r.method(), path = %r.uri().path(), request_id = request_id)
        }))
        // outermost: generates/validates the id before tracing and writes it to every response
        .layer(from_fn(request_id_middleware));
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
    use super::client_ip;
    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn req_with_xff(xff: Option<&str>, peer: Option<IpAddr>) -> Request<Body> {
        let mut b = Request::builder().uri("/bounties");
        if let Some(v) = xff {
            b = b.header("x-forwarded-for", v);
        }
        let mut req = b.body(Body::empty()).unwrap();
        if let Some(ip) = peer {
            req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip, 12345)));
        }
        req
    }

    #[test]
    fn client_ip_ignores_xff_when_untrusted() {
        let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10));
        let req = req_with_xff(Some("10.0.0.1, 203.0.113.9"), Some(peer));
        assert_eq!(client_ip(&req, false, 1), "192.0.2.10");
    }

    #[test]
    fn client_ip_takes_rightmost_proxy_observed_entry_with_spoofed_prefix() {
        let peer = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let req = req_with_xff(Some("1.2.3.4, 203.0.113.9"), Some(peer));
        assert_eq!(client_ip(&req, true, 1), "203.0.113.9");
    }

    #[test]
    fn client_ip_skips_configured_trusted_proxy_hops() {
        let peer = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        let req = req_with_xff(Some("1.2.3.4, 203.0.113.9, 198.51.100.2"), Some(peer));
        assert_eq!(client_ip(&req, true, 2), "203.0.113.9");
    }

    #[test]
    fn client_ip_falls_back_when_fewer_entries_than_hops() {
        let peer = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        let req = req_with_xff(Some("203.0.113.9"), Some(peer));
        assert_eq!(client_ip(&req, true, 2), "10.0.0.3");
    }
}
