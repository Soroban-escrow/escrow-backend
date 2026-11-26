//! `POST /github/webhooks`: verify, de-duplicate, enqueue. All real work happens in the worker.
//! A delivery that ended in error is not a duplicate: GitHub's "Redeliver" must be able to retry it.

use crate::{error::ApiError, metrics::Metrics, state::AppState};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use escrow_core::security::verify_webhook_signature;
use escrow_db::webhooks;
use escrow_engine::{dispatch::is_handled_webhook_event, jobs::enqueue_webhook};
use serde_json::json;

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub async fn receive(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
    // the signature covers the RAW body, so it is checked before anything is parsed
    if !verify_webhook_signature(&s.cfg.webhook_secret, &body, header(&headers, "x-hub-signature-256")) {
        Metrics::inc(&s.metrics.webhooks_rejected);
        return Err(ApiError::new(401, "bad_signature", "invalid signature"));
    }
    let (Some(id), Some(event)) = (header(&headers, "x-github-delivery"), header(&headers, "x-github-event")) else {
        Metrics::inc(&s.metrics.webhooks_rejected);
        return Err(ApiError::bad_request("missing delivery headers"));
    };
    if id.len() > 100 || event.len() > 60 {
        // #126 — Over-long headers are a rejection, counted like a bad
        // signature or missing headers rather than silently falling through.
        Metrics::inc(&s.metrics.webhooks_rejected);
        return Err(ApiError::bad_request("delivery headers are too long"));
    }
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        Metrics::inc(&s.metrics.webhooks_rejected);
        ApiError::new(400, "bad_json", "invalid JSON")
    })?;

    // #127 — `dispatch_webhook` drops unknown events as `"ignored"`, so storing
    // and queuing them costs a delivery row and a job for nothing. Acknowledge
    // the delivery with 202 and count it under `webhooks_ignored` instead.
    if !is_handled_webhook_event(event) {
        Metrics::inc(&s.metrics.webhooks_ignored);
        return Ok((StatusCode::ACCEPTED, Json(json!({ "ignored": true }))).into_response());
    }

    // the delivery record and its job are written together: neither can exist without the other
    let mut tx = s.db.pool.begin().await?;
    if !webhooks::insert_delivery(&mut *tx, id, event).await? {
        // mark_processed stores terminal failures in `error`; a redelivery of one of those must be
        // queued again. The conditional update is atomic, so two simultaneous redeliveries cannot both win.
        if !webhooks::retry_failed_delivery(&mut *tx, id).await? {
            Metrics::inc(&s.metrics.webhooks_duplicate);
            return Ok((StatusCode::OK, Json(json!({ "duplicate": true }))).into_response());
        }
        tracing::info!(delivery_id = %id, event = %event, "retrying a webhook whose first attempt failed");
    }
    enqueue_webhook(&mut *tx, id, event, &payload).await?;
    tx.commit().await?;
    Metrics::inc(&s.metrics.webhooks_accepted);
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response())
}
