use base64::Engine;
use cel_interpreter::extractors::{Identifier, This};
use cel_interpreter::objects::ValueType;
use cel_interpreter::{Context, ExecutionError, FunctionContext, ResolveResult, Value};
use cel_parser::Expression;
use std::sync::Arc;

use crate::cel::to_value;

pub fn insert_all(ctx: &mut Context<'_>) {
	use super::strings;

	// Custom to agentgateway
	ctx.add_function("json", json_parse);
	ctx.add_function("with", with);
	// Using the go name, base64.encode is blocked by https://github.com/cel-rust/cel-rust/issues/103 (namespacing)
	ctx.add_function("base64_encode", base64_encode);
	ctx.add_function("base64_decode", base64_decode);

	// "Strings" extension
	// https://pkg.go.dev/github.com/google/cel-go/ext#Strings
	// TODO: add support for the newer versions
	ctx.add_function("charAt", strings::char_at);
	ctx.add_function("indexOf", strings::index_of);
	ctx.add_function("join", strings::join);
	ctx.add_function("lastIndexOf", strings::last_index_of);
	ctx.add_function("lowerAscii", strings::lower_ascii);
	ctx.add_function("upperAscii", strings::upper_ascii);
	ctx.add_function("trim", strings::trim);
	ctx.add_function("replace", strings::replace);
	ctx.add_function("split", strings::split);
	ctx.add_function("substring", strings::substring);
}

pub fn base64_encode(This(this): This<Arc<String>>) -> String {
	use base64::Engine;
	base64::prelude::BASE64_STANDARD.encode(this.as_bytes())
}

pub fn base64_decode(ftx: &FunctionContext, This(this): This<Arc<String>>) -> ResolveResult {
	use base64::Engine;
	dbg!(dbg!(base64::prelude::BASE64_STANDARD
		.decode(this.as_ref()))
		.map(|v| Value::Bytes(Arc::new(v)))
		.map_err(|e| ftx.error(e)))
}

fn with(
	ftx: &FunctionContext,
	This(this): This<Value>,
	ident: Identifier,
	expr: Expression,
) -> ResolveResult {
	let mut ptx = ftx.ptx.new_inner_scope();
	ptx.add_variable_from_value(&ident, this);
	ptx.resolve(&expr)
}

fn json_parse(ftx: &FunctionContext, v: Value) -> ResolveResult {
	let sv = match v {
		Value::String(b) => serde_json::from_str(b.as_str()),
		Value::Bytes(b) => serde_json::from_slice(b.as_ref()),
		_ => return Err(ftx.error("invalid type")),
	};
	let sv: serde_json::Value = sv.map_err(|e| ftx.error(e))?;
	to_value(sv).map_err(|e| ftx.error(e))
}

#[cfg(any(test, feature = "internal_benches"))]
#[path = "functions_tests.rs"]
mod tests;
