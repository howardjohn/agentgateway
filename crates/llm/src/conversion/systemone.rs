pub mod from_decisions {
	use std::collections::HashSet;

	use async_openai::types::decisions as d;

	use crate::types::{decisions, systemone};
	use crate::{AIError, json};

	/// Request details SystemOne cannot represent, used to restore them on the response.
	/// Both sets hold SystemOne question keys.
	#[derive(Debug, Default)]
	pub struct State {
		unnamed: HashSet<String>,
		boolean_choices: HashSet<String>,
	}

	pub fn translate(req: &decisions::Request) -> Result<(Vec<u8>, State), AIError> {
		let req = json::convert::<_, d::DecisionRequest>(req)
			.map_err(|err| AIError::RequestParsing(crate::InputFormat::Decisions, err))?;
		let state = match req.input {
			d::DecisionInput::Text(text) => serde_json::Value::String(text),
			d::DecisionInput::Messages(items) => items
				.into_iter()
				.map(|d::DecisionInputItem::Message(m)| {
					let content = match m.content {
						d::DecisionInputContent::Text(text) => text,
						d::DecisionInputContent::Parts(parts) => parts
							.into_iter()
							.map(|part| match part {
								d::DecisionInputContentPart::InputText(t) => Ok(t.text),
								d::DecisionInputContentPart::InputImage(_) => Err(AIError::UnsupportedContent),
							})
							.collect::<Result<Vec<_>, _>>()?
							.join("\n"),
					};
					Ok(serde_json::json!({"role": m.role, "content": content}))
				})
				.collect::<Result<_, AIError>>()?,
		};
		let mut response_state = State::default();
		let questions = req
			.questions
			.into_iter()
			.enumerate()
			.map(|(i, q)| {
				let boolean_choice = matches!(&q, d::QuestionParam::Choice(c)
					if c.choices.iter().any(|c| matches!(c.value, d::ChoiceValue::Boolean(_))));
				let (name, question) = match q {
					d::QuestionParam::Predicate(q) => (
						q.name,
						systemone::Question::Noul {
							instructions: q.instructions.into(),
							criteria: None,
						},
					),
					d::QuestionParam::Choice(q) => (
						q.name,
						systemone::Question::Choice {
							instructions: q.instructions.into(),
							criteria: q
								.choices
								.into_iter()
								.map(|c| {
									let value = match c.value {
										d::ChoiceValue::String(s) => s,
										d::ChoiceValue::Boolean(b) => b.to_string(),
									};
									(value, c.description.unwrap_or_default())
								})
								.collect(),
						},
					),
					d::QuestionParam::Score(q) => (
						q.name,
						systemone::Question::Score {
							instructions: q.instructions.into(),
							criteria: q.levels.into_iter().map(|l| l.label).collect(),
						},
					),
				};
				let key = match name {
					Some(name) => name,
					None => {
						response_state.unnamed.insert(i.to_string());
						i.to_string()
					},
				};
				if boolean_choice {
					response_state.boolean_choices.insert(key.clone());
				}
				(key, question)
			})
			.collect();
		let req = systemone::Request {
			model: req.model,
			state,
			questions,
		};
		let body = serde_json::to_vec(&req).map_err(AIError::RequestMarshal)?;
		Ok((body, response_state))
	}

	pub fn translate_response(bytes: &[u8], state: &State) -> Result<Vec<u8>, AIError> {
		let resp: systemone::Response =
			serde_json::from_slice(bytes).map_err(AIError::ResponseParsing)?;
		let answers = resp
			.answers
			.into_iter()
			.map(|(key, answer)| {
				let choice_value = |value: String| match value.parse() {
					Ok(b) if state.boolean_choices.contains(&key) => d::ChoiceValue::Boolean(b),
					_ => d::ChoiceValue::String(value),
				};
				let name = (!state.unnamed.contains(&key)).then(|| key.clone());
				match answer {
					systemone::Answer::Noul { noul } => d::AnswerResource::Predicate(d::PredicateAnswer {
						name: name.clone(),
						probability: noul,
					}),
					systemone::Answer::Choice {
						choice,
						probabilities,
						confidence,
					} => d::AnswerResource::Choice(d::ChoiceAnswer {
						name: name.clone(),
						choice: choice_value(choice),
						probabilities: probabilities
							.into_iter()
							.map(|(value, probability)| d::ChoiceProbability {
								value: choice_value(value),
								probability,
							})
							.collect(),
						confidence,
					}),
					systemone::Answer::Score {
						score,
						legend,
						probabilities,
						confidence,
					} => d::AnswerResource::Score(d::ScoreAnswer {
						name: name.clone(),
						score,
						probabilities: probabilities
							.into_iter()
							.map(|(level, probability)| d::ScoreProbability {
								label: legend.get(&level).cloned().unwrap_or_default(),
								value: level.parse().unwrap_or_default(),
								probability,
							})
							.collect(),
						confidence,
					}),
				}
			})
			.collect();
		let (input_tokens, output_tokens) = resp
			.usage
			.map_or((0, 0), |u| (u.input_tokens, u.output_tokens));
		let resp = d::DecisionResponse {
			model: resp.model,
			answers,
			usage: d::DecisionUsage {
				input_tokens,
				input_tokens_details: Default::default(),
				output_tokens,
				output_tokens_details: Default::default(),
				total_tokens: input_tokens + output_tokens,
			},
		};
		serde_json::to_vec(&resp).map_err(AIError::ResponseMarshal)
	}
}
