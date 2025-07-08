use crate::http::jwt::Claims;
use anyhow::{Context as _, Error};
use cedar_policy::{
	Authorizer, Context, Entities, Entity, EntityId, EntityTypeName, EntityUid, Policy, PolicyId,
	PolicySet, Request, RestrictedExpression,
};
use lazy_static::lazy_static;

use crate::*;
use secrecy::SecretString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use serde_json::map::Map;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::str::FromStr;
use tracing::log;
use x509_parser::asn1_rs::AsTaggedExplicit;
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuleSet {
	#[serde(serialize_with = "se_policies", deserialize_with = "de_policies")]
	#[cfg_attr(feature = "schema", schemars(with = "serde_json::value::RawValue"))]
	pub rules: PolicySet,
	#[serde(skip)]
	authorizer: Authorizer,
}

pub fn se_policies<S: Serializer>(t: &PolicySet, serializer: S) -> Result<S::Ok, S::Error> {
	t.to_string()
		.serialize(serializer)
		.map_err(|e| serde::ser::Error::custom(e.to_string()))
}

pub fn de_policies<'de: 'a, 'a, D>(deserializer: D) -> Result<PolicySet, D::Error>
where
	D: Deserializer<'de>,
{
	let raw = Vec::<String>::deserialize(deserializer)?;
	let mut policies = PolicySet::new();
	for (idx, p) in raw.into_iter().enumerate() {
		let pp = Policy::parse(Some(PolicyId::new(format!("policy{idx}"))), p)
			.map_err(|e| serde::de::Error::custom(e.to_string()))?;
		policies
			.add(pp)
			.map_err(|e| serde::de::Error::custom(e.to_string()))?
	}
	Ok(policies)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuleSets(Vec<RuleSet>);

impl From<Vec<RuleSet>> for RuleSets {
	fn from(value: Vec<RuleSet>) -> Self {
		Self(value)
	}
}

impl RuleSets {
	pub fn validate(&self, resource: &ResourceType, claims: &Identity) -> bool {
		// If there are no rule sets, everyone has access
		if self.0.is_empty() {
			return true;
		}
		self
			.0
			.iter()
			.any(|rule_set| rule_set.validate(resource, claims))
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

impl RuleSet {
	pub fn new(rules: PolicySet) -> Self {
		Self {
			rules,
			authorizer: Authorizer::new(),
		}
	}

	// Check if the claims have access to the resource
	pub fn validate(&self, resource: &ResourceType, claims: &Identity) -> bool {
		self
			.validate_internal(resource, claims)
			.unwrap_or_else(|e| {
				tracing::warn!("authorization failed with error: {e}");
				// Fail closed
				false
			})
	}

	fn validate_internal(&self, resource: &ResourceType, claims: &Identity) -> anyhow::Result<bool> {
		tracing::debug!("Checking RBAC for resource: {:?}", resource);
		// If there are no rules, everyone has access
		if self.rules.is_empty() {
			return Ok(true);
		}

		let principal = match claims.get_claim("sub", ".") {
			Some(sub) => {
				EntityUid::from_type_name_and_id(PRINCIPAL_NAME_USER.clone(), EntityId::new(sub))
			},
			None => PRINCIPAL_UID_ANONYMOUS.clone(),
		};
		let ctx = Context::from_json_value(
			{
				let mut ctx = Map::new();
				if let Some(claim) = claims.claims.clone() {
					ctx.insert("claims".to_string(), Value::Object(claim.inner));
				};
				Value::Object(ctx)
			},
			None,
		)
		.context("failed to build context")?;

		let resource_entity = Entity::new(
			resource.entity(),
			HashMap::from([(
				"target".to_string(),
				RestrictedExpression::new_string(resource.target().to_string()),
			)]),
			HashSet::from([resource.target_entity()]),
		)
		.context("build entity")?;
		let entities = Entities::from_entities([resource_entity], None)?;
		let req = Request::new(
			principal,
			ACTION_CALL_TOOL.clone(),
			resource.entity(),
			ctx,
			None,
		)
		.context("failed to build request")?;
		tracing::trace!("authorization request: {:?}", req);
		let resp = self.authorizer.is_authorized(&req, &self.rules, &entities);

		tracing::trace!("authorization response {:?}", resp);
		Ok(matches!(resp.decision(), cedar_policy::Decision::Allow))
	}
}

fn default_key_delimiter() -> String {
	".".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub enum ResourceType {
	Tool(ResourceId),
	Prompt(ResourceId),
	Resource(ResourceId),
}

impl ResourceType {
	fn entity(&self) -> EntityUid {
		let (n, i) = match self {
			ResourceType::Tool(r) => (ENTITY_NAME_TOOL.clone(), EntityId::new(&r.id)),
			ResourceType::Prompt(r) => (ENTITY_NAME_PROMPT.clone(), EntityId::new(&r.id)),
			ResourceType::Resource(r) => (ENTITY_NAME_RESOURCE.clone(), EntityId::new(&r.id)),
		};
		EntityUid::from_type_name_and_id(n, i)
	}
	fn target_entity(&self) -> EntityUid {
		EntityUid::from_type_name_and_id(ENTITY_NAME_TARGET.clone(), EntityId::new(self.target()))
	}
	fn target(&self) -> &str {
		match self {
			ResourceType::Tool(r) => &r.target,
			ResourceType::Prompt(r) => &r.target,
			ResourceType::Resource(r) => &r.target,
		}
	}
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ResourceId {
	#[serde(default)]
	target: String,
	#[serde(default)]
	id: String,
}

impl ResourceId {
	pub fn new(target: String, id: String) -> Self {
		Self { target, id }
	}
}

#[derive(Clone, Debug, Default)]
pub struct Identity {
	pub claims: Option<Claims>,
	pub connection_id: Option<String>,
}

impl agent_core::trcng::Claim for Identity {
	fn get_claim(&self, key: &str) -> Option<&str> {
		self.get_claim(key, ".")
	}
}

impl Identity {
	pub fn empty() -> Self {
		Self {
			claims: None,
			connection_id: None,
		}
	}

	pub fn new(claims: Option<Claims>, connection_id: Option<String>) -> Self {
		Self {
			claims,
			connection_id,
		}
	}
	// Attempts to get the claim from the claims map
	// The key should be split by the key_delimiter and then the map should be searched recursively
	// If the key is not found, it returns None
	// If the key is found, it returns the value
	pub fn get_claim(&self, key: &str, key_delimiter: &str) -> Option<&str> {
		match &self.claims {
			Some(claims) => {
				// Split the key by the delimiter to handle nested lookups
				let keys = key.split(key_delimiter).collect::<Vec<&str>>();

				// Start with the root claims map
				let mut current_value = &claims.inner;

				// Navigate through each key level
				let num_keys = keys.len();
				for (index, key_part) in keys.into_iter().enumerate() {
					// Get the value at this level
					let value = current_value.get(key_part)?;

					// If this is the last key part, return the string value
					if index == num_keys - 1 {
						return value.as_str();
					}

					// Otherwise, try to navigate deeper if it's an object
					current_value = value.as_object()?;
				}

				None
			},
			None => None,
		}
	}
}

lazy_static! {
	static ref PRINCIPAL_UID_ANONYMOUS: EntityUid = "AnonymousUser::\"\"".parse().unwrap();
	static ref PRINCIPAL_NAME_USER: EntityTypeName = "User".parse().unwrap();
	static ref ACTION_CALL_TOOL: EntityUid = r#"Action::"call_tool""#.parse().unwrap();
	static ref ENTITY_NAME_TARGET: EntityTypeName = "Target".parse().unwrap();
	static ref ENTITY_NAME_TOOL: EntityTypeName = "Tool".parse().unwrap();
	static ref ENTITY_NAME_PROMPT: EntityTypeName = "Prompt".parse().unwrap();
	static ref ENTITY_NAME_RESOURCE: EntityTypeName = "Resource".parse().unwrap();
}

#[cfg(test)]
#[path = "rbac_tests.rs"]
mod tests;
