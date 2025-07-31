use crate::cel::{ContextBuilder, Error, Expression};
use cel_interpreter::Value;
use serde_json::json;

fn eval(expr: &str) -> Result<Value, Error> {
	let mut cb = ContextBuilder::new();
	let exp = Expression::new(expr)?;
	cb.register_expression(&exp);
	let exec = cb.build()?;
	exec.eval(&exp)
}

#[test]
fn with() {
	let expr = r#"[1,2].with(a, a + a)"#;
	assert_eq!(json!([1, 2, 1, 2]), eval(expr).unwrap().json().unwrap());
}

#[test]
fn json() {
	let expr = r#"json('{"hi":1}').hi"#;
	assert_eq!(json!(1), eval(expr).unwrap().json().unwrap());
}

#[test]
fn base64() {
	let expr = r#""hello".base64_encode()"#;
	assert_eq!(json!("aGVsbG8="), eval(expr).unwrap().json().unwrap());
	let expr = r#"string("hello".base64_encode().base64_decode())"#;
	assert_eq!(json!("hello"), eval(expr).unwrap().json().unwrap());
}
