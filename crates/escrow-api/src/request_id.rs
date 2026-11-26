//! Request ids: reuse a caller-supplied `x-request-id` when it is safe to echo, otherwise make one.
//!
//! The middleware is the outermost layer so every later layer (including `TraceLayer`) sees the id in
//! the request extensions. It also writes the id to the response header and adds it to API error bodies.

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderName, HeaderValue, header},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use tower_http::request_id::{MakeRequestId as _, MakeRequestUuid, RequestId};

pub const HEADER: &str = "x-request-id";
const HEADER_NAME: HeaderName = HeaderName::from_static("x-request-id");

/// Client-supplied ids are logged and echoed, so only accept a short, printable token. Anything else
/// (control characters, separators that confuse log parsers, an oversized value) is replaced by a UUID.
fn well_formed(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

/// Ensures every request has an id, propagates it to the response, and includes it in error JSON.
pub async fn middleware(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(HEADER_NAME)
        .and_then(|v| v.to_str().ok())
        .filter(|v| well_formed(v))
        .and_then(|v| HeaderValue::from_str(v).ok())
        .unwrap_or_else(|| {
            let mut make = MakeRequestUuid;
            make.make_request_id(&req).expect("a UUID is a valid header value").into_header_value()
        });

    req.headers_mut().insert(HEADER_NAME, id.clone());
    req.extensions_mut().insert(RequestId::new(id.clone()));

    let mut resp = next.run(req).await;
    resp.headers_mut().insert(HEADER_NAME, id.clone());
    if (resp.status().is_client_error() || resp.status().is_server_error())
        && resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("application/json"))
    {
        return with_request_id(resp, id).await;
    }
    resp
}

async fn with_request_id(resp: Response, id: HeaderValue) -> Response {
    let (mut parts, body) = resp.into_parts();
    let text_id = id.to_str().unwrap_or("unknown");
    match to_bytes(body, crate::BODY_LIMIT).await {
        Ok(bytes) => {
            let mut value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if let Some(error) = value.get_mut("error").and_then(|v| v.as_object_mut()) {
                error.insert("request_id".into(), json!(text_id));
            }
            parts.headers.remove(header::CONTENT_LENGTH);
            let body = serde_json::to_vec(&value).unwrap_or_default();
            Response::from_parts(parts, Body::from(body))
        }
        Err(_) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::empty())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::well_formed;

    #[test]
    fn only_short_printable_ids_are_trusted() {
        assert!(well_formed("req-123"));
        assert!(well_formed("8f14e45f-ea0a-4c3f-9a7b-1d2c3b4a5e6f"));
        assert!(!well_formed(""));
        assert!(!well_formed("bad\nheader"));
        assert!(!well_formed("a b"));
        assert!(!well_formed(&"x".repeat(65)));
    }
}
