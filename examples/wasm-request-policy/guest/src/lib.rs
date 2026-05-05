wit_bindgen::generate!({
	path: "wit",
	world: "request-policy",
	async: true,
	generate_all,
});

struct Policy;

impl exports::agentgateway::policy::policy::Guest for Policy {
	async fn apply(request: exports::agentgateway::policy::policy::Request) -> Result<bool, String> {
		let user = first_header(&request.headers, "x-user")?;

		if request.method != "GET" || request.path_with_query != "/admin" {
			return Ok(false);
		}
		if user.as_deref() != Some("admin") {
			return Ok(false);
		}

		if !agentgateway::policy::host::cel_eval_bool(
			r#"request.headers["x-user"] == "admin""#.to_string(),
		)
		.await?
		{
			return Ok(false);
		}
    return Ok(true);

		let body =
			agentgateway::policy::host::http_call("http://127.0.0.1:8081/allow".to_string()).await?;
		Ok(body == "allow")
	}
}

fn first_header(headers: &[(String, Vec<u8>)], name: &str) -> Result<Option<String>, String> {
	headers
		.iter()
		.find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
		.map(|(_, value)| String::from_utf8(value.clone()))
		.transpose()
		.map_err(|err| err.to_string())
}

export!(Policy);
