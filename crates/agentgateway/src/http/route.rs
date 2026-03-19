use std::borrow::Cow;
use std::cmp;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use agent_core::strng;

use crate::http::Request;
use crate::types::agent;
use crate::types::agent::{
	BackendReference, HeaderMatch, HeaderValueMatch, Listener, ListenerProtocol, PathMatch,
	QueryValueMatch, Route, RouteBackendReference, RouteBackendTarget, RouteName, RouteSet,
};
use crate::types::discovery::gatewayaddress::Destination;
use crate::types::discovery::{NetworkAddress, WaypointIdentity};
use crate::*;

#[cfg(any(test, feature = "internal_benches"))]
#[path = "route_test.rs"]
mod tests;

pub fn select_best_route(
	stores: Stores,
	network: Strng,
	self_addr: Option<&WaypointIdentity>,
	dst: SocketAddr,
	listener: &Listener,
	request: &Request,
) -> Option<(Arc<Route>, PathMatch)> {
	// Order:
	// * "Exact" path match.
	// * "Prefix" path match with largest number of characters.
	// * Method match.
	// * Largest number of header matches.
	// * Largest number of query param matches.
	//
	// If ties still exist across multiple Routes, matching precedence MUST be
	// determined in order of the following criteria, continuing on ties:
	//
	//  * The oldest Route based on creation timestamp.
	//  * The Route appearing first in alphabetical order by "{namespace}/{name}".
	//
	// If ties still exist within an HTTPRoute, matching precedence MUST be granted
	// to the FIRST matching rule (in list order) with a match meeting the above
	// criteria.

	let host = http::get_host(request).ok()?;
	let (default_response, host) = if matches!(listener.protocol, ListenerProtocol::HBONE) {
		let Some(self_id) = self_addr else {
			warn!("waypoint requires self address");
			return None;
		};
		// We are going to get a VIP request. Look up the Service
		let svc = stores
			.read_discovery()
			.services
			.get_by_vip(&NetworkAddress {
				network,
				address: dst.ip(),
			})?;
		let wp = svc.waypoint.as_ref()?;
		// Make sure the service is actually bound to us
		let is_ours = match &wp.destination {
			Destination::Address(addr) => {
				let stores_ref = stores.clone();
				self_id.matches_address(addr, |ns, hostname| {
					let discovery = stores_ref.read_discovery();
					let self_svc = discovery.services.get_by_namespaced_host(
						&crate::types::discovery::NamespacedHostname {
							namespace: ns.clone(),
							hostname: hostname.clone(),
						},
					)?;
					Some(self_svc.vips.clone())
				})
			},
			Destination::Hostname(n) => self_id.matches_hostname(n),
		};
		if !is_ours {
			warn!(
				"service {} is meant for waypoint {:?}, but we are {}.{}",
				svc.hostname, wp.destination, self_id.gateway, self_id.namespace
			);
			return None;
		}
		// TODO: only build this if we don't match one
		let default_route = Route {
			key: strng::literal!("_waypoint-default"),
			name: RouteName {
				name: strng::literal!("_waypoint-default"),
				namespace: svc.namespace.clone(),
				rule_name: None,
				kind: None,
			},
			hostnames: vec![],
			matches: vec![],
			inline_policies: vec![],
			backends: vec![RouteBackendReference {
				weight: 1,
				target: RouteBackendTarget::from(BackendReference::Service {
					name: svc.namespaced_hostname(),
					port: dst.port(), // TODO: get from req
				}),
				inline_policies: Vec::new(),
			}],
		};
		// If there is no route, use a default one
		let def = Some((
			Arc::new(default_route),
			PathMatch::PathPrefix(strng::new("/")),
		));
		(def, Cow::Owned(svc.hostname.to_string()))
	} else {
		(None, Cow::Borrowed(host))
	};
	for hnm in agent::HostnameMatch::all_matches(&host) {
		let mut candidates = listener.routes.get_hostname(&hnm);
		let best_match = candidates.find(|(_, m)| route_match_applies(m, request));
		if let Some((route, matcher)) = best_match {
			return Some((route, matcher.path.clone()));
		}
	}
	default_response
}

pub fn select_best_route_group(
	rg: &RouteSet,
	request: &Request,
) -> Option<(Arc<Route>, PathMatch)> {
	// Order:
	// * "Exact" path match.
	// * "Prefix" path match with largest number of characters.
	// * Method match.
	// * Largest number of header matches.
	// * Largest number of query param matches.
	//
	// If ties still exist across multiple Routes, matching precedence MUST be
	// determined in order of the following criteria, continuing on ties:
	//
	//  * The oldest Route based on creation timestamp.
	//  * The Route appearing first in alphabetical order by "{namespace}/{name}".
	//
	// If ties still exist within an HTTPRoute, matching precedence MUST be granted
	// to the FIRST matching rule (in list order) with a match meeting the above
	// criteria.

	let host = http::get_host(request).ok()?;
	for hnm in agent::HostnameMatch::all_matches(&host) {
		let mut candidates = rg.get_hostname(&hnm);
		let best_match = candidates.find(|(_, m)| route_match_applies(m, request));
		if let Some((route, matcher)) = best_match {
			return Some((route, matcher.path.clone()));
		}
	}
	None
}

pub fn best_match_for_route(route: &Route, request: &Request) -> Option<PathMatch> {
	let mut best: Option<&agent::RouteMatch> = None;
	for candidate in route
		.matches
		.iter()
		.filter(|m| route_match_applies(m, request))
	{
		if let Some(current) = best {
			if compare_route_match(candidate, current) == cmp::Ordering::Greater {
				best = Some(candidate);
			}
		} else {
			best = Some(candidate);
		}
	}
	best.map(|m| m.path.clone())
}

fn compare_route_match(a: &agent::RouteMatch, b: &agent::RouteMatch) -> cmp::Ordering {
	// Compare RouteMatch according to Gateway API sorting requirements.
	let path_rank1 = get_path_rank(&a.path);
	let path_rank2 = get_path_rank(&b.path);
	if path_rank1 != path_rank2 {
		return path_rank1.cmp(&path_rank2);
	}

	let path_len1 = get_path_length(&a.path);
	let path_len2 = get_path_length(&b.path);
	if path_len1 != path_len2 {
		return path_len1.cmp(&path_len2);
	}

	let method1 = a.method.is_some();
	let method2 = b.method.is_some();
	if method1 != method2 {
		return method1.cmp(&method2);
	}

	let header_count1 = a.headers.len();
	let header_count2 = b.headers.len();
	if header_count1 != header_count2 {
		return header_count1.cmp(&header_count2);
	}

	a.query.len().cmp(&b.query.len())
}

fn route_match_applies(m: &agent::RouteMatch, request: &Request) -> bool {
	let path_matches = match &m.path {
		PathMatch::Exact(p) => request.uri().path() == p.as_str(),
		PathMatch::Regex(r) => {
			// Regex has no defined ordering. We will order by the length of the regex expression.
			let path = request.uri().path();
			r.find(path)
				.map(|m| m.start() == 0 && m.end() == path.len())
				.unwrap_or(false)
		},
		PathMatch::PathPrefix(p) => {
			let p = p.trim_end_matches('/');
			let Some(suffix) = request.uri().path().trim_end_matches('/').strip_prefix(p) else {
				return false;
			};
			// TODO this is not right!!
			suffix.is_empty() || suffix.starts_with('/')
		},
	};
	if !path_matches {
		return false;
	}

	if let Some(method) = &m.method
		&& request.method().as_str() != method.method.as_str()
	{
		return false;
	}
	for HeaderMatch { name, value } in &m.headers {
		let Some(have) = http::get_pseudo_or_header_value(name, request) else {
			return false;
		};
		match value {
			HeaderValueMatch::Exact(want) => {
				if have.as_ref() != *want {
					return false;
				}
			},
			HeaderValueMatch::Regex(want) => {
				let Some(have_str) = have.to_str().ok() else {
					return false;
				};
				let Some(m) = want.find(have_str) else {
					return false;
				};
				if !(m.start() == 0 && m.end() == have_str.len()) {
					return false;
				}
			},
		}
	}
	let query = request
		.uri()
		.query()
		.map(|q| url::form_urlencoded::parse(q.as_bytes()).collect::<HashMap<_, _>>())
		.unwrap_or_default();
	for agent::QueryMatch { name, value } in &m.query {
		let Some(have) = query.get(name.as_str()) else {
			return false;
		};

		match value {
			QueryValueMatch::Exact(want) => {
				if have.as_ref() != want.as_str() {
					return false;
				}
			},
			QueryValueMatch::Regex(want) => {
				let Some(m) = want.find(have) else {
					return false;
				};
				if !(m.start() == 0 && m.end() == have.len()) {
					return false;
				}
			},
		}
	}
	true
}

fn get_path_rank(path: &PathMatch) -> u8 {
	match path {
		PathMatch::Exact(_) => 3,
		PathMatch::PathPrefix(_) => 2,
		PathMatch::Regex(_) => 1,
	}
}

fn get_path_length(path: &PathMatch) -> usize {
	match path {
		PathMatch::Exact(p) | PathMatch::PathPrefix(p) => p.len(),
		PathMatch::Regex(r) => r.as_str().len(),
	}
}
