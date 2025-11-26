use cel::common::ast::{CallExpr, Expr};
use cel::extractors::This;
use cel::objects::Opaque;
use cel::{Context, ExecutionError, IdedExpr, Value};
use serde::Serialize;
use std::sync::Arc;

pub fn insert_all(ctx: &mut Context<'_>) {
	ctx.add_function("precompiled_matches", PrecompileRegex::precompiled_matches)
}

pub struct DefaultOptimizer;
impl cel::Optimizer for DefaultOptimizer {
	fn specialize_call(&self, c: &CallExpr) -> Option<Expr> {
		match c.func_name.as_str() {
			"cidr" if c.args.len() == 1 && c.target.is_none() => {
				let arg = c.args.iter().next()?.clone();
				let Value::String(arg) = expr_as_value(arg)? else {
					return None;
				};
				let parsed = super::cidr::Cidr::new(&arg)?;
				Some(Expr::Inline(Value::Opaque(Arc::new(parsed))))
			},
			"ip" if c.args.len() == 1 && c.target.is_none() => {
				let arg = c.args.iter().next()?.clone();
				let Value::String(arg) = expr_as_value(arg)? else {
					return None;
				};
				let parsed = super::cidr::IP::new(&arg)?;
				Some(Expr::Inline(Value::Opaque(Arc::new(parsed))))
			},
			"matches" if c.args.len() == 1 && c.target.is_some() => {
				let t = c.target.clone()?;
				let arg = c.args.iter().next()?.clone();
				let id = arg.id;
				let Value::String(arg) = expr_as_value(arg)? else {
					return None;
				};

				// TODO: translate regex compile failures into inlined failures
				let opaque = Value::Opaque(Arc::new(PrecompileRegex(regex::Regex::new(&arg).ok()?)));
				let id_expr = IdedExpr {
					id,
					expr: Expr::Inline(opaque),
				};
				// We invert this to be 'regex.precompiled_matches(string)'
				// instead of 'string.matches(regex)'
				Some(Expr::Call(CallExpr {
					func_name: "precompiled_matches".to_string(),
					target: Some(Box::new(id_expr)),
					args: vec![*t],
				}))
			},
			_ => None,
		}
	}
}

fn expr_as_value(e: IdedExpr) -> Option<Value> {
	match e.expr {
		Expr::Literal(l) => Some(Value::from(l)),
		Expr::Inline(l) => Some(l),
		_ => None,
	}
}

#[derive(Debug, Serialize)]
struct PrecompileRegex(#[serde(with = "serde_regex")] regex::Regex);
crate::impl_opaque!(PrecompileRegex, "precompiled_regex");
impl PartialEq for PrecompileRegex {
	fn eq(&self, other: &Self) -> bool {
		self.0.as_str() == other.0.as_str()
	}
}
impl Eq for PrecompileRegex {}

impl PrecompileRegex {
	pub fn precompiled_matches(
		this: This<Arc<dyn Opaque>>,
		val: Arc<String>,
	) -> Result<bool, ExecutionError> {
		let Some(rgx) = this.0.downcast_ref::<Self>() else {
			return Err(ExecutionError::UnexpectedType {
				got: this.0.runtime_type_name().to_string(),
				want: "regex".to_string(),
			});
		};
		Ok(rgx.0.is_match(&val))
	}
}
