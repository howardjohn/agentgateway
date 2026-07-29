use agent_http::{HeaderValue, Request, header};
use agent_policy::{BoxError, PolicyContext, RequestAction, RequestPolicy};

#[derive(Debug, Default)]
pub struct Simple;

impl RequestPolicy for Simple {
	type ResponseState = ();

	async fn apply(
		&self,
		_ctx: PolicyContext<'_>,
		req: &mut Request,
	) -> Result<RequestAction<()>, BoxError> {
		req.headers_mut().insert(
			header::HeaderName::from_static("x-hello-world"),
			HeaderValue::from_static("hello"),
		);
		Ok(RequestAction::default())
	}
}

#[cfg(test)]
mod tests {
	use agent_policy::testing::{PolicyTest, TestRequest};

	use super::*;

	#[tokio::test]
	async fn inserts_hello_world_header() {
		let mut test = PolicyTest::from_policy(Simple);

		test.request(TestRequest::get("/hello")).await;
		assert_eq!(test.request_headers()["x-hello-world"], "hello");
	}
}
