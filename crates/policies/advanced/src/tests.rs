use agent_http::{Method, Uri};
use agent_policy::OutboundCallSubtype;
use agent_policy::testing::{
	PolicyTest, RecordedBackendCall, TestBackend, TestRequest, TestResponse,
};
use serde_json::json;

use super::*;

type Test = PolicyTest<Advanced<TestBackend>>;

#[tokio::test]
async fn evaluates_cel_traces_and_calls_backend() {
	let mut test = Test::new(json!({
		"condition": "request.path == \"/hello\" && claims.role.lowerAscii() == \"admin\"",
		"backend": "authz",
	}))
	.vars(json!({ "claims": { "role": "ADMIN" } }));

	test.request(TestRequest::get("/hello")).await;
	assert_eq!(
		test.backend_calls(),
		[RecordedBackendCall {
			call: OutboundCallSubtype::ExtAuthz,
			method: Method::POST,
			uri: Uri::from_static("/check"),
			..Default::default()
		}]
	);

	test.response(TestResponse::default()).await;
	assert_eq!(test.response_headers()["x-advanced-check"], "200");
}

#[tokio::test]
async fn skips_when_disabled() {
	let mut test = Test::new(json!({
		"condition": "false",
		"backend": "authz",
	}));

	test.request(TestRequest::get("/hello")).await;
	assert_eq!(test.backend_calls(), []);
}
