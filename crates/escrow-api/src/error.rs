use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use escrow_chain::ChainError;
use serde_json::json;

/// Every error leaves the API as `{ "error": { "code": "...", "message": "..." } }`.
/// `code` is stable and meant for programs; `message` is for people.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    /// The contract's error variant (`IssueAlreadyFunded`, ...) when the contract rejected the call.
    pub contract_error: Option<String>,
}

impl ApiError {
    pub fn new(status: u16, code: &str, message: &str) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            code: code.into(),
            message: message.into(),
            contract_error: None,
        }
    }
    /// A 422 for a call the contract rejected: stable `contract_error` code, the variant name and a friendly message.
    pub fn contract_rejected(name: &str) -> Self {
        let message = escrow_core::contract_errors::message_for_name(name).unwrap_or("The contract rejected this transaction.");
        let mut e = Self::new(422, "contract_error", message);
        e.contract_error = Some(name.into());
        e
    }
    pub fn with_code(mut self, code: &str) -> Self {
        self.code = code.into();
        self
    }
    pub fn bad_request(message: &str) -> Self {
        Self::new(400, "bad_request", message)
    }
    pub fn unauthenticated() -> Self {
        Self::new(401, "unauthenticated", "sign in first")
    }
    pub fn forbidden(code: &str, message: &str) -> Self {
        Self::new(403, code, message)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(404, "not_found", &format!("{what} not found"))
    }
    pub fn conflict(code: &str, message: &str) -> Self {
        Self::new(409, code, message)
    }
    pub fn rate_limited() -> Self {
        Self::new(429, "rate_limited", "too many requests; slow down")
    }
    pub fn internal() -> Self {
        Self::new(500, "internal", "something went wrong on our side")
    }

    /// A failed chain interaction, in terms the caller can act on. The contract's own error names are
    /// surfaced (`contract_IssueAlreadyFunded`); network trouble is a 503, never blamed on the caller.
    pub fn from_chain(e: &ChainError) -> Self {
        if let Some(name) = e.contract_error() {
            return Self::new(422, &format!("contract_{name}"), &format!("The contract rejected this: {name}"));
        }
        match e {
            ChainError::Archived => {
                Self::new(503, "state_archived", "On-chain state needs restoring before this can proceed; please try again later.")
            }
            e if e.is_transient() => {
                Self::new(503, "chain_unavailable", "The Stellar network is not responding; please try again shortly.")
            }
            ChainError::Invalid(m) => Self::new(422, "chain_error", m),
            _ => Self::new(422, "chain_error", "The network rejected this request."),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(name) = self.contract_error {
            error["contractError"] = json!(name);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!(error = %e, "database error");
        Self::internal()
    }
}

impl From<ChainError> for ApiError {
    fn from(e: ChainError) -> Self {
        Self::from_chain(&e)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_errors_are_named_for_the_caller() {
        let e = ApiError::from_chain(&ChainError::Simulation("HostError: Error(Contract, #9)".into()));
        assert_eq!((e.status.as_u16(), e.code.as_str()), (422, "contract_IssueAlreadyFunded"));
    }

    #[test]
    fn contract_rejections_carry_the_variant_and_a_friendly_message() {
        let e = ApiError::contract_rejected("DeadlineNotReached");
        assert_eq!((e.status.as_u16(), e.code.as_str(), e.contract_error.as_deref()), (422, "contract_error", Some("DeadlineNotReached")));
        assert!(e.message.contains("grace period"));
        assert_eq!(ApiError::contract_rejected("Mystery").message, "The contract rejected this transaction.");
    }

    #[test]
    fn network_trouble_is_a_503_not_a_user_error() {
        assert_eq!(ApiError::from_chain(&ChainError::Http("timeout".into())).status.as_u16(), 503);
        assert_eq!(ApiError::from_chain(&ChainError::Busy).code, "chain_unavailable");
        assert_eq!(ApiError::from_chain(&ChainError::Archived).code, "state_archived");
    }

    #[test]
    fn helpful_validation_messages_pass_through_but_internals_do_not() {
        let e = ApiError::from_chain(&ChainError::Invalid("account G… does not exist on this network; fund it first".into()));
        assert_eq!((e.status.as_u16(), e.message.contains("fund it first")), (422, true));
        let secret = ApiError::from_chain(&ChainError::Decode("secret internal detail".into()));
        assert!(!secret.message.contains("internal detail"));
    }
}
