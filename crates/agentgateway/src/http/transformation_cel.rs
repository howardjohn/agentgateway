use crate::cel;
use crate::cel::{Attribute, ContextBuilder, ExpressionContext};
use crate::*;
use ::http::{HeaderName, HeaderValue, Request};
use agent_core::prelude::Strng;
use http_body_util::BodyExt;
use minijinja::value::Object;
use minijinja::{Environment, Value, context};
use serde_with::{serde_as, SerializeAs, TryFromInto};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

const REQUEST_HEADER_ATTRIBUTE: &str = "request_header";
const RESPONSE_HEADER_ATTRIBUTE: &str = "header";
const BODY_ATTRIBUTE: &str = "body";
const RESPONSE_ATTRIBUTE: &str = "response";
const JWT_ATTRIBUTE: &str = "jwt";
const MCP_ATTRIBUTE: &str = "mcp";

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LocalTransformationConfig {
	#[serde(default, skip_serializing_if = "is_default")]
	pub request: Option<LocalTransform>,
	#[serde(default, skip_serializing_if = "is_default")]
	pub response: Option<LocalTransform>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LocalTransform {
	#[serde(default, skip_serializing_if = "is_default")]
	pub add: Vec<(Strng, Strng)>,
	#[serde(default, skip_serializing_if = "is_default")]
	pub set: Vec<(Strng, Strng)>,
	#[serde(default, skip_serializing_if = "is_default")]
	pub remove: Vec<Strng>,
	#[serde(default, skip_serializing_if = "is_default")]
	pub body: Option<Strng>,
}

impl TryFrom<LocalTransformationConfig> for Transformation {
	type Error = anyhow::Error;

	fn try_from(value: LocalTransformationConfig) -> Result<Self, Self::Error> {
		let LocalTransformationConfig { request, response } = value;
		let request = if let Some(req) = request {
			let set = req
				.set
				.into_iter()
				.map(|(k, v)| {
					let tk = HeaderName::try_from(k.as_str())?;
					let tv = cel::Expression::new(v.as_str())?;
					Ok::<_, anyhow::Error>((tk, tv))
				})
				.collect::<Result<_, _>>()?;
			TransformerConfig {
				set,
				..Default::default()
			}
		} else {
			Default::default()
		};
		Ok(Transformation {
			request: Arc::new(request),
			response: Default::default(),
		})
	}
}

#[derive(Clone, Debug, Serialize)]
pub struct Transformation {
	request: Arc<TransformerConfig>,
	response: Arc<TransformerConfig>,
}

#[serde_as]
#[derive(Debug, Default, Serialize)]
pub struct TransformerConfig {
	#[serde_as(serialize_as = "serde_with::Map<SerAsStr, _>")]
	pub add: Vec<(HeaderName, cel::Expression)>,
	#[serde_as(serialize_as = "serde_with::Map<SerAsStr, _>")]
	pub set: Vec<(HeaderName, cel::Expression)>,
	#[serde_as(serialize_as = "Vec<SerAsStr>")]
	pub remove: Vec<HeaderName>,
	pub body: Option<cel::Expression>,
}

impl Transformation {
	pub fn ctx(&self) -> ContextBuilder {
		ContextBuilder {
			attributes: todo!(),
			context: Default::default(),
		}
	}
}

pub struct SerAsStr;
impl<T> SerializeAs<T> for SerAsStr
where
T: AsRef<str>,
{
	fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
	where
	S: Serializer,
	{
		source
		.as_ref()
		.serialize(serializer)
	}
}

impl Transformation {
	pub fn apply(&self, req: &mut crate::http::Request, ctx: ContextBuilder) {
		//     let v = to_value(ctx);
		//     for t in self.templates.iter() {
		//         let tmpl = self.env.get_template(&t.name).expect("template must exist");
		//         let headers = req.headers();
		//         let res = tmpl.render(context! {
		// 			STATE => v,
		// 	});
		//         req.headers_mut().insert(
		//             t.header.clone(),
		//             HeaderValue::try_from(res.unwrap_or_else(|_| "template render failed".to_string()))
		//               .unwrap(),
		//         );
		//     }
	}
}

#[cfg(test)]
#[path = "transformation_cel_tests.rs"]
mod tests;
