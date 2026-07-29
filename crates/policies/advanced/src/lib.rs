use agent_core::define_schema_aliases;
use agent_core::prelude::info;
use agent_core::serdes::apply;
use agent_http::{
	Body, HeaderName, HeaderValue, Method, PolicyResponse, Request, Response, StatusCode, Uri,
};
use agent_policy::{
	BoxError, Expression, OutboundCallSubtype, PolicyContext, RequestAction, RequestPolicy,
	TraceSeverity, pol_event, pol_result, policy_trace,
};

const TRACE_POLICY_KIND: &str = "advanced";

define_schema_aliases!();

#[apply(schema!)]
pub struct Advanced<T> {
	pub condition: Arc<Expression>,
	#[serde(flatten)]
	pub backend: T,
}

impl<T> RequestPolicy for Advanced<T>
where
	T: Send + Sync + 'static,
{
	/// Status returned by the check backend, echoed on the response.
	type ResponseState = StatusCode;

	async fn apply(
		&self,
		ctx: PolicyContext<'_>,
		req: &mut Request,
	) -> Result<RequestAction<StatusCode>, BoxError> {
		let enabled = match ctx.cel.eval_request(self.condition.as_ref(), req)? {
			cel::Value::Bool(enabled) => enabled,
			other => {
				return Err(
					std::io::Error::other(format!(
						"advanced policy condition returned {other:?}, expected bool"
					))
					.into(),
				);
			},
		};
		if !enabled {
			info!("skipping request");
			return Ok(RequestAction::default());
		}
		info!("handling request");

		let start = policy_trace().timed_start();
		pol_event!(TraceSeverity::Info, "calling advanced policy backend");
		let mut callout = Request::new(Body::empty());
		*callout.method_mut() = Method::POST;
		*callout.uri_mut() = Uri::from_static("/check");
		let response = ctx
			.backend
			.ok_or_else(|| std::io::Error::other("advanced policy requires a backend dispatcher"))?
			.send(OutboundCallSubtype::ExtAuthz, callout)
			.await?;
		pol_result!(
			TraceSeverity::Info,
			start,
			"advanced policy backend returned {}",
			response.status()
		);

		Ok(RequestAction::with_response_state(response.status()))
	}

	async fn apply_response(
		&self,
		_ctx: PolicyContext<'_>,
		status: StatusCode,
		resp: &mut Response,
	) -> Result<PolicyResponse, BoxError> {
		resp.headers_mut().insert(
			HeaderName::from_static("x-advanced-check"),
			HeaderValue::from(status.as_u16()),
		);
		Ok(PolicyResponse::default())
	}

	fn expressions(&self) -> impl Iterator<Item = &Expression> {
		std::iter::once(self.condition.as_ref())
	}
}

#[cfg(test)]
mod tests;
use std::sync::Arc;
