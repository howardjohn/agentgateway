use std::cell::RefCell;
use std::collections::HashMap;

use http::{HeaderName, HeaderValue, Request};
use minijinja::value::Object;
use minijinja::{Environment, Value, context};

pub struct Transformation {
	env: Environment<'static>,
	request_headers: HashMap<HeaderName, String>,
}

fn build(transforms: HashMap<HeaderName, String>) -> anyhow::Result<Transformation> {
	let mut env: Environment<'static> = Environment::new();
	let mut res = HashMap::new();
	for (k, t) in transforms.into_iter() {
		let name = format!("request_header_{k}");
		env.add_template_owned(name.clone(), t)?;
		env.add_function("request_header", functions::request_header);
		// }
		res.insert(k, name);
	}
	Ok(Transformation {
		env,
		request_headers: res,
	})
}

#[derive(Debug)]
struct RequestState {
	req: crate::http::HeaderMap,
}

impl Object for RequestState {}

// thread_local! {
//     static CURRENT_REQUEST: RefCell<Option<&'a crate::http::Request>> = RefCell::default()
// }
//
// /// Binds the given request to a thread local for `url_for`.
// fn with_bound_req<F, R>(req: &crate::http::Request, f: F) -> R
// where
// F: FnOnce() -> R,
// {
//     let rq = std::rc::Rc::new(req);
// 	CURRENT_REQUEST.with(|current_req| *current_req.borrow_mut() = Some(req.clone()));
// 	let rv = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
// 	CURRENT_REQUEST.with(|current_req| current_req.borrow_mut().take());
// 	match rv {
// 		Ok(rv) => rv,
// 		Err(panic) => std::panic::resume_unwind(panic),
// 	}
// }

impl Transformation {
	pub fn apply(&self, req: &mut crate::http::Request) {
		for (name, tmpl_key) in self.request_headers.iter() {
			let tmpl = self
				.env
				.get_template(tmpl_key)
				.expect("template must exist");
			let headers = req.headers();
			// This is rather unfortunate we need to copy things when they may not even be used
			// We could use undeclared_variables to find what is reference but we still need to clone the full header map
			let res = tmpl.render(context! {
					STATE => Value::from_object(RequestState{req: req.headers().clone()}),
			});
			req.headers_mut().insert(
				name,
				HeaderValue::try_from(res.unwrap_or_else(|_| "template render failed".to_string()))
					.unwrap(),
			);
		}
	}
}

mod functions {
	use minijinja::{State, Value};

	use crate::http::transformation::RequestState;

	pub fn request_header(state: &State, key: &str) -> String {
		let Some(state) = state.lookup("STATE") else {
			return "".to_string();
		};
		let Some(state) = state.downcast_object_ref::<RequestState>() else {
			return "".to_string();
		};
		state
			.req
			.get(key)
			.and_then(|s| {
				std::str::from_utf8(s.as_bytes())
					.ok()
					.map(|s| s.to_string())
			})
			.unwrap_or("".to_string())
	}
}

#[cfg(test)]
#[path = "transformation_tests.rs"]
mod tests;
