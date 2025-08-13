use cel::Value;
use serde_json::json;

use crate::cel::{ContextBuilder, Error, Expression};

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
fn random() {
	let expr = r#"int(random() * 10.0)"#;
	let v = eval(expr).unwrap().json().unwrap().as_i64().unwrap();
	assert!((0..=10).contains(&v));
}

#[test]
fn base64() {
	let expr = r#""hello".base64_encode()"#;
	assert_eq!(json!("aGVsbG8="), eval(expr).unwrap().json().unwrap());
	let expr = r#"string("hello".base64_encode().base64_decode())"#;
	assert_eq!(json!("hello"), eval(expr).unwrap().json().unwrap());
}

#[test]
fn map_values() {
	let expr = r#"{"a": 1, "b": 2}.map_values(v, v * 2)"#;
	assert_eq!(json!({"a": 2, "b": 4}), eval(expr).unwrap().json().unwrap());
}
