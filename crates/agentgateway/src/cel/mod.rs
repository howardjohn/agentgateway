// Portions of this code are heavily inspired from https://github.com/Kuadrant/wasm-shim/
// Under Apache 2.0 license (https://github.com/Kuadrant/wasm-shim/blob/main/LICENSE)

use crate::serdes::*;
use bytes::Bytes;
use cel_interpreter::extractors::{Arguments, This};
use cel_interpreter::objects::{Key, Map, ValueType};
use cel_interpreter::{Context, ExecutionError, Program, ResolveResult, Value};
use cel_parser::{Expression as CelExpression, ParseError};
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum Error {
	#[error("execution: {0}")]
	Resolve(#[from] ExecutionError),
	#[error("parse: {0}")]
	Parse(#[from] ParseError),
	#[error("variable: {0}")]
	Variable(Box<dyn std::error::Error>),
}

pub struct Expression {
	attributes: Vec<Attribute>,
	expression: CelExpression,
	root_context: Context<'static>,
}

impl Expression {
	pub fn new(expression: &str) -> Result<Self, Error> {
		let expression = cel_parser::parse(expression)?;

		let mut props = Vec::with_capacity(5);
		properties(&expression, &mut props, &mut Vec::default());

		let mut attributes: Vec<Attribute> = props
			.into_iter()
			.map(|tokens| {
				let path = Path::new(tokens);
				// known_attribute_for(&path).unwrap_or(Attribute {
				Attribute {
					path,
					cel_type: None,
				}
			})
			.collect();

		attributes.sort_by(|a, b| a.path.tokens().len().cmp(&b.path.tokens().len()));

		Ok(Self {
			attributes,
			expression,
			root_context: Context::default(),
			// extended,
		})
	}

	pub fn eval(&self, req: &crate::http::Request) -> Result<Value, Error> {
		let mut ctx = self.root_context.new_inner_scope();

		// Putting the full request into the context means serializing and copying state, that may not be referenced.
		// Instead, we will inspect what variables are referenced and only load those.
		let mut ec = ExpressionContext::default();
		for attribute in &self.attributes {
			match attribute.path.tokens.first().map(|s| s.as_str()) {
				Some("request") => {
					ec.request = Some(RequestContext {
						method: req.method(),
						// TODO: split headers and the rest?
						headers: req.headers(),
						uri: req.uri(),
					})
				},
				_ => {},
			}
		}

		let ExpressionContext { request } = ec;
		if let Some(r) = request {
			ctx.add_variable("request", r).map_err(Error::Variable);
		}

		Ok(Value::resolve(&self.expression, &ctx)?)
	}
}

#[derive(Clone, Debug, Default, Serialize)]
struct ExpressionContext<'a> {
	request: Option<RequestContext<'a>>,
}

#[derive(Clone, Debug, Serialize)]
struct RequestContext<'a> {
	#[serde(serialize_with = "ser_debug")]
	method: &'a ::http::Method,

	#[serde(with = "http_serde::uri")]
	uri: &'a ::http::Uri,

	#[serde(with = "http_serde::header_map")]
	headers: &'a ::http::HeaderMap,
}

fn create_context<'a>() -> Context<'a> {
	let mut ctx = Context::default();
	ctx
}

fn properties<'e>(exp: &'e CelExpression, all: &mut Vec<Vec<&'e str>>, path: &mut Vec<&'e str>) {
	match exp {
		CelExpression::Arithmetic(e1, _, e2)
		| CelExpression::Relation(e1, _, e2)
		| CelExpression::Ternary(e1, _, e2)
		| CelExpression::Or(e1, e2)
		| CelExpression::And(e1, e2) => {
			properties(e1, all, path);
			properties(e2, all, path);
		},
		CelExpression::Unary(_, e) => {
			properties(e, all, path);
		},
		CelExpression::Member(e, a) => {
			if let cel_parser::Member::Attribute(attr) = &**a {
				path.insert(0, attr.as_str())
			}
			properties(e, all, path);
		},
		CelExpression::FunctionCall(_, target, args) => {
			// The attributes of the values returned by functions are skipped.
			path.clear();
			if let Some(target) = target {
				properties(target, all, path);
			}
			for e in args {
				properties(e, all, path);
			}
		},
		CelExpression::List(e) => {
			for e in e {
				properties(e, all, path);
			}
		},
		CelExpression::Map(v) => {
			for (e1, e2) in v {
				properties(e1, all, path);
				properties(e2, all, path);
			}
		},
		CelExpression::Atom(_) => {},
		CelExpression::Ident(v) => {
			if !path.is_empty() {
				path.insert(0, v.as_str());
				all.push(path.clone());
				path.clear();
			}
		},
	}
}

pub struct Attribute {
	path: Path,
	cel_type: Option<ValueType>,
}

impl Debug for Attribute {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Attribute {{ {:?} }}", self.path)
	}
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub struct Path {
	tokens: Vec<String>,
}

impl Display for Path {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{}",
			self
				.tokens
				.iter()
				.map(|t| t.replace('.', "\\."))
				.collect::<Vec<String>>()
				.join(".")
		)
	}
}

impl Debug for Path {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "path: {:?}", self.tokens)
	}
}

impl From<&str> for Path {
	fn from(value: &str) -> Self {
		let mut token = String::new();
		let mut tokens: Vec<String> = Vec::new();
		let mut chars = value.chars();
		while let Some(ch) = chars.next() {
			match ch {
				'.' => {
					tokens.push(token);
					token = String::new();
				},
				'\\' => {
					if let Some(next) = chars.next() {
						token.push(next);
					}
				},
				_ => token.push(ch),
			}
		}
		tokens.push(token);

		Self { tokens }
	}
}

impl Path {
	pub fn new<T: Into<String>>(tokens: Vec<T>) -> Self {
		Self {
			tokens: tokens.into_iter().map(|i| i.into()).collect(),
		}
	}
	pub fn tokens(&self) -> Vec<&str> {
		self.tokens.iter().map(String::as_str).collect()
	}
}

#[cfg(any(test, feature = "internal_benches"))]
pub mod tests {
	use super::*;
	use crate::http::Body;
	use crate::store::Stores;
	use crate::types::agent::{Listener, ListenerProtocol, PathMatch, Route, RouteMatch, RouteSet};
	use agent_core::strng;
	use divan::Bencher;
	use http::Method;
	use std::net::{IpAddr, Ipv4Addr, SocketAddr};

	#[test]
	fn expression() {
		let expr =
			Expression::new(r#"request.method == "GET" && request.headers["x-example"] == "value""#)
				.unwrap();
		let req = ::http::Request::builder()
			.method(Method::GET)
			.uri("http://example.com")
			.header("x-example", "value")
			.body(Body::empty())
			.unwrap();
		assert_eq!(Value::Bool(true), expr.eval(&req).unwrap());
	}

	#[divan::bench]
	fn bench_with_request(b: Bencher) {
		let expr =
			Expression::new(r#"request.method == "GET" && request.headers["x-example"] == "value""#)
				.unwrap();
		b.with_inputs(|| {
			::http::Request::builder()
				.method(Method::GET)
				.uri("http://example.com")
				.header("x-example", "value")
				.body(Body::empty())
				.unwrap()
		})
		.bench_refs(|r| {
			expr.eval(r).unwrap();
		});
	}
	#[divan::bench]
	fn bench(b: Bencher) {
		let expr = Expression::new(r#"1 + 2 == 3"#).unwrap();
		b.with_inputs(|| {
			::http::Request::builder()
				.method(Method::GET)
				.uri("http://example.com")
				.header("x-example", "value")
				.body(Body::empty())
				.unwrap()
		})
		.bench_refs(|r| {
			expr.eval(r).unwrap();
		});
	}
}
