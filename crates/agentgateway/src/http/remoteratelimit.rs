use std::collections::HashMap;
use std::convert::Infallible;
use std::time::SystemTime;

use ::http::uri::Authority;
use ::http::{HeaderMap, StatusCode, Uri};
use anyhow::anyhow;
use bytes::Bytes;
use http_body::Frame;
use http_body_util::BodyStream;
use itertools::Itertools;
use prost_types::Timestamp;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::cel::{Executor, Expression};
use crate::client::{Client, Transport};
use crate::control::AuthSource;
use crate::http::backendtls::BackendTLS;
use crate::http::ext_proc::GrpcReferenceChannel;
use crate::http::filters::DirectResponse;
use crate::http::remoteratelimit::proto::RateLimitDescriptor;
use crate::http::remoteratelimit::proto::rate_limit_descriptor::Entry;
use crate::http::remoteratelimit::proto::rate_limit_service_client::RateLimitServiceClient;
use crate::http::transformation_cel::Transformation;
use crate::http::{HeaderName, HeaderValue, PolicyResponse, Request, Response};
use crate::mcp::rbac::PolicySet;
use crate::proxy::ProxyError;
use crate::proxy::httpproxy::PolicyClient;
use crate::transport::stream::{TCPConnectionInfo, TLSConnectionInfo};
use crate::types::agent;
use crate::types::agent::{Backend, SimpleBackendReference, Target};
use crate::*;

#[allow(warnings)]
#[allow(clippy::derive_partial_eq_without_eq)]
pub mod proto {
	tonic::include_proto!("envoy.service.ratelimit.v3");
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRateLimit {
	pub domain: String,
	pub target: Arc<SimpleBackendReference>,
	pub descriptors: Arc<DescriptorSet>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor(String, cel::Expression);

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct DescriptorSet(
	#[serde(deserialize_with = "de_descriptors")]
	#[cfg_attr(feature = "schema", schemars(with = "Vec<Vec<KV>>"))]
	Arc<Vec<Vec<Descriptor>>>,
);

#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
struct KV {
	key: String,
	value: String,
}

fn de_descriptors<'de: 'a, 'a, D>(deserializer: D) -> Result<Arc<Vec<Vec<Descriptor>>>, D::Error>
where
	D: Deserializer<'de>,
{
	let raw = Vec::<Vec<KV>>::deserialize(deserializer)?;
	let parsed: Vec<_> = raw
		.into_iter()
		.map(|r| {
			r.into_iter()
				.map(|i| cel::Expression::new(i.value).map(|v| Descriptor(i.key, v)))
				.collect::<Result<Vec<_>, _>>()
		})
		.collect::<Result<_, _>>()
		.map_err(|e| serde::de::Error::custom(e.to_string()))?;
	Ok(Arc::new(parsed))
}

impl RemoteRateLimit {
	pub async fn check(
		&self,
		client: PolicyClient,
		req: &mut Request,
		exec: &Executor<'_>,
	) -> Result<PolicyResponse, ProxyError> {
		let mut descriptors = Vec::with_capacity(self.descriptors.0.len());
		for entries in self.descriptors.0.iter() {
			if let Some(rl_entries) = Self::eval_descriptor(exec, entries) {
				descriptors.push(RateLimitDescriptor {
					entries: rl_entries,
					limit: None,
					hits_addend: None,
				});
			}
		}
		let request = proto::RateLimitRequest {
			domain: self.domain.clone(),
			descriptors,
			hits_addend: 0,
		};

		trace!("connecting to {:?}", self.target);
		let chan = GrpcReferenceChannel {
			target: self.target.clone(),
			client,
		};
		let mut client = RateLimitServiceClient::new(chan);
		let resp = client.should_rate_limit(request).await;
		trace!("check response: {:?}", resp);
		if let Err(ref error) = resp {
			warn!("rate limit request failed: {:?}", error);
		}
		let cr = resp.map_err(|_| ProxyError::RateLimitFailed)?;

		let cr = cr.into_inner();

		let mut res = PolicyResponse::default();
		// if not OK, we directly respond
		if cr.overall_code != (proto::rate_limit_response::Code::Ok as i32) {
			let mut rb = ::http::response::Builder::new().status(StatusCode::TOO_MANY_REQUESTS);
			if let Some(hm) = rb.headers_mut() {
				process_headers(hm, cr.response_headers_to_add)
			}
			let resp = rb
				.body(http::Body::from(cr.raw_body))
				.map_err(|e| ProxyError::Processing(e.into()))?;
			res.direct_response = Some(resp);
			return Ok(res);
		}

		process_headers(req.headers_mut(), cr.request_headers_to_add);
		if !cr.response_headers_to_add.is_empty() {
			let mut hm = HeaderMap::new();
			process_headers(&mut hm, cr.response_headers_to_add);
			res.response_headers = Some(hm);
		}
		Ok(res)
	}

	fn eval_descriptor(exec: &Executor, entries: &Vec<Descriptor>) -> Option<Vec<Entry>> {
		let mut rl_entries = Vec::with_capacity(entries.len());
		for Descriptor(k, lookup) in entries {
			// We drop the entire set if we cannot eval one
			let value = exec.eval(lookup).ok()?;
			let cel_interpreter::Value::String(value) = value else {
				return None;
			};
			let entry = Entry {
				key: k.clone(),
				value: value.to_string(),
			};
			rl_entries.push(entry);
		}
		Some(rl_entries)
	}

	pub fn expressions(&self) -> impl Iterator<Item = &Expression> {
		self.descriptors.0.iter().flatten().map(|v| &v.1)
	}
}

fn process_headers(hm: &mut HeaderMap, headers: Vec<proto::HeaderValue>) {
	for h in headers {
		let Ok(hn) = HeaderName::from_bytes(h.key.as_bytes()) else {
			continue;
		};
		let hv = if h.raw_value.is_empty() {
			HeaderValue::from_bytes(h.key.as_bytes())
		} else {
			HeaderValue::from_bytes(&h.raw_value)
		};
		let Ok(hv) = hv else {
			continue;
		};
		hm.insert(hn, hv);
	}
}
