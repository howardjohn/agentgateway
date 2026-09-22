use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::yaml;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Policy {
	Disabled,
	Name(String),
	Pair(String, u16),
	Nested { policies: Vec<Policy> },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
	policies: BTreeMap<String, Option<Policy>>,
}

#[test]
fn recursive_enum_maps() {
	let input = r#"policies:
  route:
    nested:
      policies:
      - disabled
      - name: 'true'
      - pair: [upstream, 8080]
      - nested:
          policies:
          - name: '123'
  missing: null
"#;
	let expected = Config {
		policies: BTreeMap::from([
			(
				"route".into(),
				Some(Policy::Nested {
					policies: vec![
						Policy::Disabled,
						Policy::Name("true".into()),
						Policy::Pair("upstream".into(), 8080),
						Policy::Nested {
							policies: vec![Policy::Name("123".into())],
						},
					],
				}),
			),
			("missing".into(), None),
		]),
	};
	assert_eq!(yaml::from_str::<Config>(input).unwrap(), expected);
	let output = yaml::to_string(&expected).unwrap();
	assert_eq!(yaml::from_str::<Config>(&output).unwrap(), expected);
	assert_eq!(
		yaml::from_str::<serde_json::Value>(&output).unwrap(),
		serde_json::to_value(&expected).unwrap(),
	);
	let json = serde_json::to_string(&expected).unwrap();
	assert_eq!(yaml::from_str::<Config>(&json).unwrap(), expected);
}

#[test]
fn invalid_config_reports_nested_path() {
	let err = yaml::from_str::<Config>("policies:\n  route:\n    pair: [upstream, invalid]\n")
		.unwrap_err()
		.to_string();
	assert!(err.contains("policies.route"), "{err}");
	assert!(err.contains("invalid"), "{err}");
	assert!(yaml::from_str::<Config>("policies: {}\nunknown: true").is_err());
	assert!(yaml::from_str::<Config>("policies: [").is_err());
}
