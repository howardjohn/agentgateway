use cel::common::ast::{Expr, operators};
use cel::common::value::CelVal;

/// Recover a public model name from a provider model and a supported CEL transformation.
/// Supports `llmRequest.model`, literal prefix/suffix addition and stripping, and chains
/// of these operations. Stripping is reversed by restoring the affix; callers must
/// check the resulting candidate against the configured model pattern.
/// Returns `None` for unsupported expressions or a mismatched added affix.
pub fn reverse_model_transformation(
	expression: &crate::cel::Expression,
	model: &str,
) -> Option<String> {
	fn literal(expr: &Expr) -> Option<&str> {
		// CEL compilation turns string literals into Inline values during optimization.
		match expr {
			Expr::Literal(CelVal::String(value)) => Some(value.as_str()),
			Expr::Inline(cel::Value::String(value)) => Some(value.as_ref()),
			_ => None,
		}
	}

	fn reverse(expr: &Expr, model: &str) -> Option<String> {
		// Undo the outer operation first, then recurse toward llmRequest.model.
		// For "vendor/" + llmRequest.model.stripPrefix("public/"), this removes
		// "vendor/" from the provider model before restoring "public/".
		match expr {
			// llmRequest.model: the identity transformation and recursion's base case.
			Expr::Select(select)
				if !select.test
					&& select.field == "model"
					&& matches!(&select.operand.expr, Expr::Ident(name) if name == "llmRequest") =>
			{
				Some(model.to_owned())
			},
			Expr::Call(call) => match (
				call.func_name.as_str(),
				call.target.as_deref(),
				call.args.as_slice(),
			) {
				// llmRequest.model.stripPrefix("openai/"): "gpt-4o" -> "openai/gpt-4o".
				("stripPrefix", Some(target), [affix]) => {
					let prefix = literal(&affix.expr)?;
					reverse(&target.expr, &format!("{prefix}{model}"))
				},
				// llmRequest.model.stripSuffix("-public"): "gpt-4o" -> "gpt-4o-public".
				("stripSuffix", Some(target), [affix]) => {
					let suffix = literal(&affix.expr)?;
					reverse(&target.expr, &format!("{model}{suffix}"))
				},
				// "openai/" + llmRequest.model, or llmRequest.model + "-latest".
				// Reversal removes the added literal; a missing affix means no match.
				(operators::ADD, None, [left, right]) => {
					if let Some(prefix) = literal(&left.expr) {
						reverse(&right.expr, model.strip_prefix(prefix)?)
					} else if let Some(suffix) = literal(&right.expr) {
						reverse(&left.expr, model.strip_suffix(suffix)?)
					} else {
						None
					}
				},
				// llmRequest["model"]: bracket notation for the same identity transformation.
				(operators::INDEX, None, [object, field])
					if matches!(&object.expr, Expr::Ident(name) if name == "llmRequest")
						&& literal(&field.expr) == Some("model") =>
				{
					Some(model.to_owned())
				},
				_ => None,
			},
			_ => None,
		}
	}

	reverse(&expression.ast().expr, model)
}

#[cfg(test)]
mod tests {
	use rstest::rstest;

	use super::*;

	#[rstest]
	#[case::identity("llmRequest.model", "gpt-4o", Some("gpt-4o"))]
	#[case::index("llmRequest['model']", "gpt-4o", Some("gpt-4o"))]
	#[case::strip_prefix(
		"llmRequest.model.stripPrefix('openai/')",
		"gpt-4o",
		Some("openai/gpt-4o")
	)]
	#[case::strip_suffix(
		"llmRequest.model.stripSuffix('-public')",
		"gpt-4o",
		Some("gpt-4o-public")
	)]
	#[case::add_prefix("'openai/' + llmRequest.model", "openai/gpt-4o", Some("gpt-4o"))]
	#[case::add_suffix("llmRequest.model + '-latest'", "gpt-4o-latest", Some("gpt-4o"))]
	#[case::missing_prefix("'openai/' + llmRequest.model", "gpt-4o", None)]
	#[case::missing_suffix("llmRequest.model + '-latest'", "gpt-4o", None)]
	#[case::rename_prefix(
		"'vendor/' + llmRequest.model.stripPrefix('public/')",
		"vendor/gpt-4o",
		Some("public/gpt-4o")
	)]
	#[case::strip_both(
		"llmRequest.model.stripPrefix('openai/').stripSuffix('-public')",
		"gpt-4o",
		Some("openai/gpt-4o-public")
	)]
	#[case::add_both(
		"'openai/' + llmRequest.model + '-latest'",
		"openai/gpt-4o-latest",
		Some("gpt-4o")
	)]
	#[case::empty_affix("llmRequest.model.stripPrefix('') + ''", "gpt-4o", Some("gpt-4o"))]
	#[case::empty_model("'openai/' + llmRequest.model", "openai/", Some(""))]
	#[case::unicode("llmRequest.model.stripPrefix('模型/')", "gpt-4o", Some("模型/gpt-4o"))]
	#[case::escaped_literal(
		r#"llmRequest.model.stripPrefix('team\'s/')"#,
		"gpt-4o",
		Some("team's/gpt-4o")
	)]
	#[case::constant("'gpt-4o'", "gpt-4o", None)]
	#[case::other_field("llmRequest.other", "gpt-4o", None)]
	#[case::other_object("request.model", "gpt-4o", None)]
	#[case::other_index("llmRequest['other']", "gpt-4o", None)]
	#[case::presence("has(llmRequest.model)", "gpt-4o", None)]
	#[case::dynamic_affix(
		"llmRequest.model.stripPrefix(request.headers['x-prefix'])",
		"gpt-4o",
		None
	)]
	#[case::dynamic_add("request.headers['x-prefix'] + llmRequest.model", "gpt-4o", None)]
	#[case::repeated_model("llmRequest.model + llmRequest.model", "gpt-4ogpt-4o", None)]
	#[case::unsupported_call("llmRequest.model.lowerAscii()", "gpt-4o", None)]
	#[case::conditional("llmRequest.model == 'public' ? 'gpt-4o' : 'other'", "gpt-4o", None)]
	fn reverse_transformation(
		#[case] expression: &str,
		#[case] upstream_model: &str,
		#[case] expected: Option<&str>,
	) {
		let expression = crate::cel::Expression::new_strict(expression).unwrap();
		let reversed = reverse_model_transformation(&expression, upstream_model);
		assert_eq!(reversed.as_deref(), expected);

		if let Some(public_model) = reversed {
			let request = serde_json::json!({"model": public_model});
			let executor = crate::cel::Executor::new_llm(None, &request);
			assert_eq!(
				executor.eval(&expression).unwrap().json().unwrap(),
				serde_json::json!(upstream_model),
				"recovered model must transform back to the upstream model"
			);
		}
	}
}
