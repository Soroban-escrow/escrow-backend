use crate::error::ApiError;
use axum::{
    Json,
    extract::{FromRequest, Request},
};
use serde::de::DeserializeOwned;

/// The caller's address as the edge middleware determined it (honours `X-Forwarded-For` only when configured to).
#[derive(Clone, Debug)]
pub struct ClientIp(pub String);

/// JSON body that rejects in the API's own error format, without echoing parser internals.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(_) => Err(ApiError::bad_request("invalid or missing JSON body")),
        }
    }
}
