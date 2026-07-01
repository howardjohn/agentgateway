use std::fs::File;
use std::io::Write;
use std::sync::{Arc, RwLock};

use agent_xds::{Handler, XdsResource, XdsUpdate};
use agentgateway::store::{BindStore, BindStoreUpdater, DiscoveryStore, DiscoveryStoreUpdater};
use bytes::Bytes;
use divan::Bencher;
use protos::agent;
use protos::agent::resource::Kind as AgentKind;
use protos::workload;
use protos::workload::address::Type as WorkloadAddressType;
// #[global_allocator]
// static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
	eprintln!("Benchmarking...");
	#[cfg(all(not(test), not(feature = "internal_benches")))]
	panic!("benches must have -F internal_benches");
	#[allow(unused)]
	use agentgateway as _;
	with_profiling(divan::main);
}
#[divan::bench()]
fn bench(b: Bencher) {
	b.bench(|| {});
}

#[divan::bench(args = [1_000usize, 10_000])]
fn discovery_xds_workload_apply(bencher: Bencher, workload_count: usize) {
	let service = xds_service();
	let workloads = (0..workload_count).map(xds_workload).collect::<Vec<_>>();

	bencher.bench_local(move || {
		let updater = DiscoveryStoreUpdater::new(Arc::new(RwLock::new(DiscoveryStore::new())));
		let mut updates = std::iter::once(XdsUpdate::Update(XdsResource {
			name: "default/bench.default.svc.cluster.local".into(),
			resource: workload::Address {
				r#type: Some(WorkloadAddressType::Service(service.clone())),
			},
		}))
		.chain(workloads.iter().cloned().map(|workload| {
			XdsUpdate::Update(XdsResource {
				name: workload.uid.clone().into(),
				resource: workload::Address {
					r#type: Some(WorkloadAddressType::Workload(workload)),
				},
			})
		}));
		let updates: Box<&mut dyn Iterator<Item = XdsUpdate<workload::Address>>> =
			Box::new(&mut updates);
		updater.handle(updates).expect("xds update accepted");
	});
}

#[divan::bench(args = [1_000usize, 10_000])]
fn bind_xds_route_apply(bencher: Bencher, route_count: usize) {
	let bind = xds_bind();
	let listener = xds_listener();
	let routes = (0..route_count).map(xds_route).collect::<Vec<_>>();

	bencher.bench_local(move || {
		let updater = BindStoreUpdater::new(Arc::new(RwLock::new(BindStore::new(
			true,
			agentgateway::ThreadingMode::Multithreaded,
			Default::default(),
		))));
		let mut updates = std::iter::once(XdsUpdate::Update(XdsResource {
			name: bind.key.clone().into(),
			resource: agent::Resource {
				kind: Some(AgentKind::Bind(bind.clone())),
			},
		}))
		.chain(std::iter::once(XdsUpdate::Update(XdsResource {
			name: listener.key.clone().into(),
			resource: agent::Resource {
				kind: Some(AgentKind::Listener(listener.clone())),
			},
		})))
		.chain(routes.iter().cloned().map(|route| {
			XdsUpdate::Update(XdsResource {
				name: route.key.clone().into(),
				resource: agent::Resource {
					kind: Some(AgentKind::Route(route)),
				},
			})
		}));
		let updates: Box<&mut dyn Iterator<Item = XdsUpdate<agent::Resource>>> = Box::new(&mut updates);
		updater.handle(updates).expect("xds update accepted");
	});
}

fn xds_service() -> workload::Service {
	workload::Service {
		name: "bench".to_string(),
		namespace: "default".to_string(),
		hostname: "bench.default.svc.cluster.local".to_string(),
		ports: vec![workload::Port {
			service_port: 80,
			target_port: 8080,
			..Default::default()
		}],
		..Default::default()
	}
}

fn xds_workload(i: usize) -> workload::Workload {
	let ip = 1 + (i as u32);
	workload::Workload {
		uid: format!("cluster//Pod/default/bench-{i}"),
		name: format!("bench-{i}"),
		namespace: "default".to_string(),
		addresses: vec![Bytes::copy_from_slice(&[
			((ip >> 24) & 0xff) as u8,
			((ip >> 16) & 0xff) as u8,
			((ip >> 8) & 0xff) as u8,
			(ip & 0xff) as u8,
		])],
		network: "default".to_string(),
		services: [(
			"default/bench.default.svc.cluster.local".to_string(),
			workload::PortList {
				ports: vec![workload::Port {
					service_port: 80,
					target_port: 8080,
					..Default::default()
				}],
			},
		)]
		.into_iter()
		.collect(),
		status: workload::WorkloadStatus::Healthy as i32,
		..Default::default()
	}
}

fn xds_bind() -> agent::Bind {
	agent::Bind {
		key: "bench-bind".to_string(),
		port: 8080,
		protocol: agent::bind::Protocol::Http as i32,
		tunnel_protocol: agent::bind::TunnelProtocol::Direct as i32,
		mode: agent::bind::Mode::Internal as i32,
	}
}

fn xds_listener() -> agent::Listener {
	agent::Listener {
		key: "bench-listener".to_string(),
		bind_key: "bench-bind".to_string(),
		name: Some(agent::ListenerName {
			gateway_name: "bench-gateway".to_string(),
			gateway_namespace: "default".to_string(),
			listener_name: "http".to_string(),
			listener_set: None,
		}),
		hostname: "bench.example.com".to_string(),
		protocol: agent::Protocol::Http as i32,
		tls: None,
	}
}

fn xds_route(i: usize) -> agent::Route {
	agent::Route {
		key: format!("bench-route-{i}"),
		listener_key: "bench-listener".to_string(),
		name: Some(agent::RouteName {
			kind: "HTTPRoute".to_string(),
			name: format!("bench-route-{i}"),
			namespace: "default".to_string(),
			rule_name: None,
		}),
		hostnames: vec![format!("bench-{i}.example.com")],
		..Default::default()
	}
}

#[cfg(not(target_family = "unix"))]
pub fn with_profiling(f: impl FnOnce()) {
	f()
}

#[cfg(target_family = "unix")]
pub fn with_profiling(f: impl FnOnce()) {
	use pprof::protos::Message;
	let guard = pprof::ProfilerGuardBuilder::default()
		.frequency(1000)
		.build()
		.unwrap();

	f();
	eprintln!("Writing profile to /tmp/pprof-agentgateway.prof...");

	let report = guard.report().build().unwrap();
	let profile = report.pprof().unwrap();

	let body = profile.write_to_bytes().unwrap();
	File::create("/tmp/pprof-agentgateway.prof")
		.unwrap()
		.write_all(&body)
		.unwrap()
}
