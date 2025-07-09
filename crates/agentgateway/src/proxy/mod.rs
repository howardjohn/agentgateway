mod gateway;
pub mod httpproxy;
pub mod tcpproxy;

use crate::http::Body;
use crate::http::HeaderValue;
use crate::http::Response;
use crate::http::StatusCode;
use crate::*;
pub use gateway::Gateway;
use hyper_util_fork::client::legacy::Error as HyperError;

#[derive(thiserror::Error, Debug)]
pub enum ProxyError {
	#[error("bind not found")]
	BindNotFound,
	#[error("listener not found")]
	ListenerNotFound,
	#[error("route not found")]
	RouteNotFound,
	#[error("no valid backends")]
	NoValidBackends,
	#[error("backends does not exist")]
	BackendDoesNotExist,
	#[error("backends required DNS resolution which failed")]
	DnsResolution,
	#[error("failed to apply filters: {0}")]
	FilterError(#[from] http::filters::Error),
	#[error("backend type cannot be used in mirror")]
	BackendUnsupportedMirror,
	#[error("authentication failure: {0}")]
	JwtAuthenticationFailure(http::jwt::TokenError),
	#[error("service not found")]
	ServiceNotFound,
	#[error("no healthy backends")]
	NoHealthyEndpoints,
	#[error("authorization failed")]
	AuthorizationFailed,
	#[error("backend authentication failed: {0}")]
	BackendAuthenticationFailed(anyhow::Error),
	#[error("upstream call failed: {0:?}")]
	UpstreamCallFailed(HyperError),
	#[error("request timeout")]
	RequestTimeout,
	#[error("processing failed: {0}")]
	Processing(anyhow::Error),
	#[error("processing failed: {0}")]
	ProcessingString(String),
	#[error("rate limit exceeded")]
	RateLimitExceeded,
	#[error("rate limit failed")]
	RateLimitFailed,
	#[error("invalid request")]
	InvalidRequest,
	#[error("request upgrade failed, backend tried {1:?} but {0:?} was requested")]
	UpgradeFailed(Option<HeaderValue>, Option<HeaderValue>),
}

impl ProxyError {
	#[allow(clippy::match_like_matches_macro)]
	pub fn is_retryable(&self) -> bool {
		match self {
			ProxyError::UpstreamCallFailed(_) => true,
			ProxyError::RequestTimeout => true,
			ProxyError::DnsResolution => true,
			_ => false,
		}
	}
	pub fn as_response(&self) -> Response {
		let code = match self {
			ProxyError::BindNotFound => StatusCode::NOT_FOUND,
			ProxyError::ListenerNotFound => StatusCode::NOT_FOUND,
			ProxyError::RouteNotFound => StatusCode::NOT_FOUND,
			ProxyError::NoValidBackends => StatusCode::INTERNAL_SERVER_ERROR,
			ProxyError::BackendDoesNotExist => StatusCode::INTERNAL_SERVER_ERROR,
			ProxyError::BackendUnsupportedMirror => StatusCode::INTERNAL_SERVER_ERROR,
			ProxyError::ServiceNotFound => StatusCode::INTERNAL_SERVER_ERROR,
			ProxyError::BackendAuthenticationFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,

			ProxyError::UpgradeFailed(_, _) => StatusCode::BAD_GATEWAY,

			// Should it be 4xx?
			ProxyError::FilterError(_) => StatusCode::INTERNAL_SERVER_ERROR,
			ProxyError::InvalidRequest => StatusCode::BAD_REQUEST,

			ProxyError::JwtAuthenticationFailure(_) => StatusCode::FORBIDDEN,
			ProxyError::AuthorizationFailed => StatusCode::FORBIDDEN,

			ProxyError::DnsResolution => StatusCode::SERVICE_UNAVAILABLE,
			ProxyError::NoHealthyEndpoints => StatusCode::SERVICE_UNAVAILABLE,
			ProxyError::UpstreamCallFailed(_) => StatusCode::SERVICE_UNAVAILABLE,

			ProxyError::RequestTimeout => StatusCode::GATEWAY_TIMEOUT,
			ProxyError::Processing(_) => StatusCode::SERVICE_UNAVAILABLE,
			ProxyError::ProcessingString(_) => StatusCode::SERVICE_UNAVAILABLE,
			ProxyError::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
			ProxyError::RateLimitFailed => StatusCode::TOO_MANY_REQUESTS,
		};
		let msg = self.to_string();
		::http::Response::builder()
			.status(code)
			.header(hyper::header::CONTENT_TYPE, "text/plain")
			.body(http::Body::from(msg))
			.unwrap()
	}
}
