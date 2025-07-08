use super::*;
use assert_matches::assert_matches;
use cedar_policy::{Policy, PolicyId, PolicySet};
use secrecy::SecretString;
use serde_json::{Map, Value};

fn create_policy_set(policies: Vec<&str>) -> PolicySet {
	let mut policy_set = PolicySet::new();
	for (idx, policy_str) in policies.into_iter().enumerate() {
		let policy = Policy::parse(Some(PolicyId::new(format!("policy{idx}"))), policy_str)
			.expect("Failed to parse policy");
		policy_set.add(policy).expect("Failed to add policy to set");
	}
	policy_set
}

#[test]
fn test_rbac_reject_exact_match() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { context.claims.user == "admin" };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	headers.insert("sub".to_string(), "1234567890".to_string().into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(false)
	);
}

#[test]
fn test_rbac_check_exact_match() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { context.claims.sub == "1234567890" };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	headers.insert("sub".to_string(), "1234567890".to_string().into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(true)
	);
}

#[test]
fn test_rbac_target() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { resource.target == "server" };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	headers.insert("sub".to_string(), "1234567890".to_string().into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(true)
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"not-server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(false)
	);
}

#[test]
fn test_rbac_check_contains_match() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { context.claims.groups == "admin" };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	// Use a simple string that matches exactly
	headers.insert("groups".to_string(), "admin".to_string().into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(true)
	);
}

#[test]
fn test_rbac_check_nested_key_match() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { context.claims.user.role == "admin" };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	let mut user_obj = Map::new();
	user_obj.insert("role".to_string(), "admin".into());
	headers.insert("user".to_string(), user_obj.into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(true)
	);
}

#[test]
fn test_rbac_check_array_contains_match() {
	let policies = vec![
		r#"permit(principal, action == Action::"call_tool", resource == Tool::"increment") when { context.claims.roles.contains("admin") };"#,
	];
	let rbac = RuleSet::new(create_policy_set(policies));
	let mut headers = Map::new();
	// Create an array of roles
	let roles: Vec<Value> = vec!["user".into(), "admin".into(), "developer".into()];
	headers.insert("roles".to_string(), roles.into());
	let id = Identity::new(
		Some(Claims {
			inner: headers,
			jwt: SecretString::new("".into()),
		}),
		None,
	);
	assert_matches!(
		rbac.validate_internal(
			&ResourceType::Tool(ResourceId::new(
				"server".to_string(),
				"increment".to_string()
			)),
			&id
		),
		Ok(true)
	);
}
