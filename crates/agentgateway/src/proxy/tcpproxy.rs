use std::net::SocketAddr;
use std::sync::Arc;

use rand::prelude::IndexedRandom;

use crate::http::Request;
use crate::proxy::httpproxy::BackendCall;
use crate::proxy::{ProxyError, httpproxy};
use crate::store::{BackendPolicies, RoutePath};
use crate::telemetry::log;
use crate::telemetry::log::{DropOnLog, RequestLog};
use crate::telemetry::metrics::TCPLabels;
use crate::transport::stream::{Socket, TCPConnectionInfo, TLSConnectionInfo};
use crate::types::agent;
use crate::types::agent::{
	BackendPolicy, BackendReference, BindKey, Listener, ListenerProtocol, Route,
	RouteBackendReference, RouteName, SimpleBackend, SimpleBackendReference,
	SimpleBackendWithPolicies, TCPRoute, TCPRouteBackend, TCPRouteBackendReference,
	TransportProtocol,
};
use crate::types::discovery::gatewayaddress::Destination;
use crate::types::discovery::{NamespacedHostname, NetworkAddress};
use crate::{ProxyInputs, *};

#[derive(Clone)]
pub struct TCPProxy {
	pub(super) bind_name: BindKey,
	pub(super) inputs: Arc<ProxyInputs>,
	pub(super) selected_listener: Arc<Listener>,
	#[allow(unused)]
	pub(super) target_address: SocketAddr,
}

impl TCPProxy {
	pub async fn proxy(&self, connection: Socket) {
		let start = Instant::now();

		let tcp = connection
			.ext::<TCPConnectionInfo>()
			.expect("tcp connection must be set");
		let mut log: DropOnLog = RequestLog::new(
			log::CelLogging::new(
				self.inputs.cfg.logging.clone(),
				self.inputs.cfg.tracing.clone(),
			),
			self.inputs.metrics.clone(),
			start,
			tcp.clone(),
		)
		.into();
		let ret = self.proxy_internal(connection, log.as_mut().unwrap()).await;
		if let Err(e) = ret {
			log.with(|l| l.error = Some(e.to_string()));
		}
	}

	async fn proxy_internal(
		&self,
		connection: Socket,
		log: &mut RequestLog,
	) -> Result<(), ProxyError> {
		log.tls_info = connection.ext::<TLSConnectionInfo>().cloned();
		log.backend_protocol = Some(cel::BackendProtocol::tcp);
		let tcp_labels = TCPLabels {
			bind: Some(&self.bind_name).into(),
			gateway: Some(&self.selected_listener.name.as_gateway_name()).into(),
			listener: self.selected_listener.name.listener_name.clone().into(),
			protocol: if log.tls_info.is_some() {
				TransportProtocol::tls
			} else {
				TransportProtocol::tcp
			},
		};
		self
			.inputs
			.metrics
			.downstream_connection
			.get_or_create(&tcp_labels)
			.inc();
		let sni = log
			.tls_info
			.as_ref()
			.and_then(|tls| tls.server_name.as_deref());

		let selected_listener = self.selected_listener.clone();
		let inputs = self.inputs.clone();
		let bind_name = self.bind_name.clone();
		debug!(bind=%bind_name, "route for bind");
		log.bind_name = Some(bind_name.clone());
		log.listener_name = Some(selected_listener.name.clone());
		debug!(bind=%bind_name, listener=%selected_listener.key, "selected listener");

		let selected_route = select_best_route(
			sni,
			inputs.stores.clone(),
			inputs.cfg.network.clone(),
			inputs.cfg.self_addr.clone(),
			self.target_address,
			&selected_listener,
		)
		.ok_or(ProxyError::RouteNotFound)?;
		log.route_name = Some(selected_route.name.clone());

		let route_path = RoutePath {
			route: &selected_route.name,
			listener: &selected_listener.name,
		};

		debug!(bind=%bind_name, listener=%selected_listener.key, route=%selected_route.key, "selected route");
		let selected_backend =
			select_tcp_backend(selected_route.as_ref()).ok_or(ProxyError::NoValidBackends)?;
		let selected_backend = resolve_backend(selected_backend, self.inputs.as_ref())?;
		let backend_policies = get_backend_policies(
			&self.inputs,
			&selected_backend.backend,
			&selected_backend.inline_policies,
			route_path,
		);

		let backend_call = match &selected_backend.backend.backend {
			SimpleBackend::Service(svc, port) => httpproxy::build_service_call(
				inputs.as_ref(),
				backend_policies,
				&mut Some(log),
				None,
				svc,
				port,
			)?,
			SimpleBackend::Opaque(_, target) => BackendCall {
				target: target.clone(),
				http_version_override: None,
				transport_override: None,
				network_gateway: None,
				backend_policies,
			},
			SimpleBackend::Invalid => return Err(ProxyError::BackendDoesNotExist),
		};

		let bi = selected_backend.backend.backend.backend_info();
		log.endpoint = Some(backend_call.target.clone());
		log.backend_info = Some(bi);

		let transport = crate::proxy::httpproxy::build_transport(
			&inputs,
			&backend_call,
			backend_call.backend_policies.backend_tls.clone(),
			// TODO: for TCP we should actually probably do something here: telling it to not use ALPN at all?
			None,
		)
		.await?;

		// export rx/tx bytes on drop
		let mut connection = connection;
		connection.set_transport_metrics(self.inputs.metrics.clone(), tcp_labels);

		inputs
			.upstream
			.call_tcp(client::TCPCall {
				source: connection,
				target: backend_call.target,
				transport,
			})
			.await?;
		Ok(())
	}
}

fn select_best_route(
	host: Option<&str>,
	stores: Stores,
	network: Strng,
	self_addr: Option<Strng>,
	dst: SocketAddr,
	listener: &Listener,
) -> Option<Arc<TCPRoute>> {
	// TCP matching is much simpler than HTTP.
	// We pick the best matching hostname, else fallback to precedence:
	//
	//  * The oldest Route based on creation timestamp.
	//  * The Route appearing first in alphabetical order by "{namespace}/{name}".

	// Assume matches are ordered already (not true today)
	if matches!(listener.protocol, ListenerProtocol::HBONE)
		&& listener.tcp_routes.is_empty()
		&& listener.routes.is_empty()
	{
		let Some(self_addr) = self_addr else {
			warn!("waypoint requires self address");
			return None;
		};
		// We are going to get a VIP request. Look up the Service
		// TODO: add a mode to fallback to a DFP backend
		let svc = stores
			.read_discovery()
			.services
			.get_by_vip(&NetworkAddress {
				network,
				address: dst.ip(),
			})?;
		let wp = svc.waypoint.as_ref()?;
		// Make sure the service is actually bound to us. TODO: should we have a more explicit setup?
		match &wp.destination {
			Destination::Address(aadr) => {
				// TODO: this is pretty sketchy
				let Some(ns) = self_addr.split(".").nth(1) else {
					warn!("waypoint cannot find self namespace");
					return None;
				};
				let self_svc =
					stores
						.read_discovery()
						.services
						.get_by_namespaced_host(&NamespacedHostname {
							namespace: ns.into(),
							hostname: self_addr,
						})?;
				if !self_svc.vips.contains(aadr) {
					warn!(
						"service {} is meant for waypoint {}, but we are {:?}",
						svc.hostname, aadr, self_svc.vips,
					);
				}
			},
			Destination::Hostname(n) => {
				if n.hostname != self_addr {
					warn!(
						"service {} is meant for waypoint {}, but we are {}",
						svc.hostname, n.hostname, self_addr
					);
					return None;
				}
			},
		}
		// TODO: only build this if we don't match one
		let default_route = TCPRoute {
			key: strng::literal!("_waypoint-default"),
			name: RouteName {
				name: strng::literal!("_waypoint-default"),
				namespace: svc.namespace.clone(),
				rule_name: None,
				kind: None,
			},
			hostnames: vec![],
			backends: vec![TCPRouteBackendReference {
				weight: 1,
				backend: SimpleBackendReference::Service {
					name: svc.namespaced_hostname(),
					port: dst.port(),
				},
				inline_policies: Vec::new(),
			}],
		};
		return Some(Arc::new(default_route));
	};
	for hnm in agent::HostnameMatch::all_matches_or_none(host) {
		if let Some(r) = listener.tcp_routes.get_hostname(&hnm) {
			return Some(Arc::new(r.clone()));
		}
	}
	None
}

fn select_tcp_backend(route: &TCPRoute) -> Option<TCPRouteBackendReference> {
	route
		.backends
		.choose_weighted(&mut rand::rng(), |b| b.weight)
		.ok()
		.cloned()
}

fn resolve_backend(
	b: TCPRouteBackendReference,
	pi: &ProxyInputs,
) -> Result<TCPRouteBackend, ProxyError> {
	let backend = super::resolve_simple_backend(&b.backend, pi)?;
	Ok(TCPRouteBackend {
		weight: b.weight,
		backend,
		inline_policies: b.inline_policies,
	})
}

pub fn get_backend_policies(
	inputs: &ProxyInputs,
	backend: &SimpleBackendWithPolicies,
	inline_policies: &[BackendPolicy],
	route_path: RoutePath,
) -> BackendPolicies {
	inputs.stores.read_binds().backend_policies(
		backend.backend.target(),
		&[&backend.inline_policies, inline_policies],
		Some(route_path),
	)
}
