use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{cmp, env};

use agent_core::prelude::*;
use anyhow::anyhow;
use hickory_resolver::config::ResolveHosts;

use crate::control::caclient;
use crate::telemetry::log::LoggingFields;
use crate::telemetry::trc;
use crate::types::discovery::Identity;
use crate::{
	Address, Config, ConfigSource, NestedRawConfig, RawConfig, XDSConfig, cel, client, serdes,
	telemetry,
};

pub fn parse_config(contents: String, filename: Option<PathBuf>) -> anyhow::Result<Config> {
	let nested: NestedRawConfig = serdes::yamlviajson::from_str(&contents)?;
	let raw = nested.config.unwrap_or_default();

	let ipv6_enabled = parse::<bool>("IPV6_ENABLED")?
		.or(raw.enable_ipv6)
		.unwrap_or(true);
	let ipv6_localhost_enabled = if ipv6_enabled {
		// IPv6 may be generally enabled, but not on localhost. In that case, we do not want to bind on IPv6.
		crate::ipv6_enabled_on_localhost().unwrap_or_else(|e| {
			warn!(err=?e, "failed to determine if IPv6 was disabled; continuing anyways, but this may fail");
			true
		})
	} else {
		false
	};
	let bind_wildcard = if ipv6_enabled {
		IpAddr::V6(Ipv6Addr::UNSPECIFIED)
	} else {
		IpAddr::V4(Ipv4Addr::UNSPECIFIED)
	};
	let local_config = parse::<PathBuf>("LOCAL_XDS_PATH")?
		.or(raw.local_xds_path)
		.or(filename)
		.map(ConfigSource::File);

	let (resolver_cfg, mut resolver_opts) = hickory_resolver::system_conf::read_system_conf()?;

	let xds = {
		let address = validate_uri(empty_to_none(parse("XDS_ADDRESS")?).or(raw.xds_address))?;
		// if local_config.is_none() && address.is_none() {
		// 	anyhow::bail!("file or XDS configuration is required")
		// }
		let (namespace, gateway) = if address.is_some() {
			(
				parse("NAMESPACE")?
					.or(raw.namespace.clone())
					.context("NAMESPACE is required")?,
				parse("GATEWAY")?
					.or(raw.gateway)
					.context("GATEWAY is required")?,
			)
		} else {
			("".to_string(), "".to_string())
		};
		XDSConfig {
			address,
			namespace,
			gateway,
			local_config,
		}
	};

	let self_addr = if !xds.namespace.is_empty() && !xds.gateway.is_empty() {
		// TODO: this is bad
		Some(strng::format!(
			"{}.{}.svc.cluster.local",
			xds.gateway,
			xds.namespace
		))
	} else {
		None
	};
	let ca_address = validate_uri(empty_to_none(parse("CA_ADDRESS")?))?;
	let ca = if let Some(addr) = ca_address {
		let td = parse("TRUST_DOMAIN")?
			.or(raw.trust_domain)
			.unwrap_or("cluster.local".to_string());
		let ns = parse("NAMESPACE")?
			.or(raw.namespace)
			.context("NAMESPACE is required")?;
		let sa = parse("SERVICE_ACCOUNT")?
			.or(raw.service_account)
			.context("SERVICE_ACCOUNT is required")?;
		let cluster: String = parse("CLUSTER_ID")?
			.or(raw.cluster_id)
			.unwrap_or("Kubernetes".to_string());
		let tok = parse("AUTH_TOKEN")?.or(raw.auth_token);
		let auth = match tok {
			None => {
				// If nothing is set, conditionally use the default if it exists
				if Path::new(&"./var/run/secrets/tokens/istio-token").exists() {
					crate::control::AuthSource::Token(
						PathBuf::from("./var/run/secrets/tokens/istio-token"),
						cluster.clone(),
					)
				} else {
					crate::control::AuthSource::None
				}
			},
			Some(p) if Path::new(&p).exists() => {
				// This is a file
				crate::control::AuthSource::Token(PathBuf::from(p), cluster.clone())
			},
			Some(p) => {
				anyhow::bail!("auth token {p} not found")
			},
		};
		let ca_cert = parse_default(
			"CA_ROOT_CA",
			"./var/run/secrets/istio/root-cert.pem".to_string(),
		)?;
		let ca_root_cert = if Path::new(&ca_cert).exists() {
			crate::control::RootCert::File(ca_cert.into())
		} else if ca_cert.eq("SYSTEM") {
			// handle SYSTEM special case for ca
			crate::control::RootCert::Default
		} else {
			crate::control::RootCert::Default
		};
		Some(caclient::Config {
			address: addr,
			secret_ttl: Duration::from_secs(86400),
			identity: Identity::Spiffe {
				trust_domain: td.into(),
				namespace: ns.into(),
				service_account: sa.into(),
			},

			auth,
			ca_cert: ca_root_cert,
		})
	} else {
		None
	};
	let network = parse("NETWORK")?.or(raw.network).unwrap_or_default();
	let termination_min_deadline = parse_duration("CONNECTION_MIN_TERMINATION_DEADLINE")?
		.or(raw.connection_min_termination_deadline)
		.unwrap_or_default();
	let termination_max_deadline =
		parse_duration("CONNECTION_TERMINATION_DEADLINE")?.or(raw.connection_min_termination_deadline);
	let otlp = empty_to_none(parse("OTLP_ENDPOINT")?)
		.or(raw.tracing.as_ref().map(|t| t.otlp_endpoint.clone()));
	// Parse admin_addr from environment variable or config file
	let admin_addr = parse::<String>("ADMIN_ADDR")?
		.or(raw.admin_addr)
		.map(|addr| Address::new(ipv6_localhost_enabled, &addr))
		.transpose()?
		.unwrap_or(Address::Localhost(ipv6_localhost_enabled, 0));

	Ok(crate::Config {
		network: network.into(),
		admin_addr,
		stats_addr: Address::SocketAddr(SocketAddr::new(bind_wildcard, 0)),
		readiness_addr: Address::SocketAddr(SocketAddr::new(bind_wildcard, 0)),
		self_addr,
		xds,
		ca,
		num_worker_threads: parse_worker_threads()?,
		termination_min_deadline,
		termination_max_deadline: match termination_max_deadline {
			Some(period) => period,
			None => match parse::<u64>("TERMINATION_GRACE_PERIOD_SECONDS")? {
				// We want our drain period to be less than Kubernetes, so we can use the last few seconds
				// to abruptly terminate anything remaining before Kubernetes SIGKILLs us.
				// We could just take the SIGKILL, but it is even more abrupt (TCP RST vs RST_STREAM/TLS close, etc)
				// Note: we do this in code instead of in configuration so that we can use downward API to expose this variable
				// if it is added to Kubernetes (https://github.com/kubernetes/kubernetes/pull/125746).
				Some(secs) => Duration::from_secs(cmp::max(
					if secs > 10 {
						secs - 5
					} else {
						// If the grace period is really low give less buffer
						secs - 1
					},
					1,
				)),
				None => Duration::from_secs(5),
			},
		},
		tracing: trc::Config {
			endpoint: otlp,
			fields: Arc::new(
				raw
					.tracing
					.and_then(|f| f.fields)
					.map(|fields| {
						Ok::<_, anyhow::Error>(LoggingFields {
							remove: fields.remove.into_iter().collect(),
							add: fields
								.add
								.iter()
								.map(|(k, v)| cel::Expression::new(v).map(|v| (k.clone(), Arc::new(v))))
								.collect::<Result<_, _>>()?,
						})
					})
					.transpose()?
					.unwrap_or_default(),
			),
		},
		logging: telemetry::log::Config {
			filter: raw
				.logging
				.as_ref()
				.and_then(|l| l.filter.as_ref())
				.map(cel::Expression::new)
				.transpose()?
				.map(Arc::new),
			fields: Arc::new(
				raw
					.logging
					.and_then(|f| f.fields)
					.map(|fields| {
						Ok::<_, anyhow::Error>(LoggingFields {
							remove: fields.remove.into_iter().collect(),
							add: fields
								.add
								.iter()
								.map(|(k, v)| cel::Expression::new(v).map(|v| (k.clone(), Arc::new(v))))
								.collect::<Result<_, _>>()?,
						})
					})
					.transpose()?
					.unwrap_or_default(),
			),
		},
		dns: client::Config {
			// TODO: read from file
			resolver_cfg,
			resolver_opts,
		},
		proxy_metadata: crate::ProxyMetadata {
			instance_ip: std::env::var("INSTANCE_IP").unwrap_or_else(|_| "1.1.1.1".to_string()),
			pod_name: std::env::var("POD_NAME").unwrap_or_else(|_| "".to_string()),
			pod_namespace: std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "".to_string()),
			node_name: std::env::var("NODE_NAME").unwrap_or_else(|_| "".to_string()),
			role: format!(
				"{ns}~{name}",
				ns = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "".to_string()),
				name = std::env::var("GATEWAY").unwrap_or_else(|_| "".to_string())
			),
			node_id: format!(
				"agentgateway~{ip}~{pod_name}.{ns}~{ns}.svc.cluster.local",
				ip = std::env::var("INSTANCE_IP").unwrap_or_else(|_| "1.1.1.1".to_string()),
				pod_name = std::env::var("POD_NAME").unwrap_or_else(|_| "".to_string()),
				ns = std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "".to_string())
			),
		},
		hbone: Arc::new(agent_hbone::Config {
			// window size: per-stream limit
			window_size: parse("HTTP2_STREAM_WINDOW_SIZE")?
				.or(raw.http2.as_ref().and_then(|h| h.window_size))
				.unwrap_or(4u32 * 1024 * 1024),
			// connection window size: per connection.
			// Setting this to the same value as window_size can introduce deadlocks in some applications
			// where clients do not read data on streamA until they receive data on streamB.
			// If streamA consumes the entire connection window, we enter a deadlock.
			// A 4x limit should be appropriate without introducing too much potential buffering.
			connection_window_size: parse("HTTP2_CONNECTION_WINDOW_SIZE")?
				.or(raw.http2.as_ref().and_then(|h| h.connection_window_size))
				.unwrap_or(16u32 * 1024 * 1024),
			frame_size: parse("HTTP2_FRAME_SIZE")?
				.or(raw.http2.as_ref().and_then(|h| h.frame_size))
				.unwrap_or(1024u32 * 1024),

			pool_max_streams_per_conn: parse("POOL_MAX_STREAMS_PER_CONNECTION")?
				.or(raw.http2.as_ref().and_then(|h| h.pool_max_streams_per_conn))
				.unwrap_or(100u16),

			pool_unused_release_timeout: parse_duration("POOL_UNUSED_RELEASE_TIMEOUT")?
				.or(
					raw
						.http2
						.as_ref()
						.and_then(|h| h.pool_unused_release_timeout),
				)
				.unwrap_or(Duration::from_secs(60 * 5)),
		}),
	})
}

fn parse<T: FromStr>(env: &str) -> anyhow::Result<Option<T>>
where
	<T as FromStr>::Err: ToString,
{
	match env::var(env) {
		Ok(val) => val
			.parse()
			.map(|v| Some(v))
			.map_err(|e: <T as FromStr>::Err| {
				anyhow::anyhow!("invalid env var {}={} ({})", env, val, e.to_string())
			}),
		Err(_) => Ok(None),
	}
}

fn parse_default<T: FromStr>(env: &str, default: T) -> anyhow::Result<T>
where
	<T as FromStr>::Err: std::error::Error + Sync + Send,
{
	parse(env).map(|v| v.unwrap_or(default))
}

fn parse_duration(env: &str) -> anyhow::Result<Option<Duration>> {
	parse::<String>(env)?
		.map(|ds| {
			duration_str::parse(&ds)
				.map_err(|e| anyhow::anyhow!("invalid env var {}={} ({})", env, ds, e))
		})
		.transpose()
}
fn parse_duration_default(env: &str, default: Duration) -> anyhow::Result<Duration> {
	parse_duration(env).map(|v| v.unwrap_or(default))
}

pub fn empty_to_none<A: AsRef<str>>(inp: Option<A>) -> Option<A> {
	if let Some(inner) = &inp {
		if inner.as_ref().is_empty() {
			return None;
		}
	}
	inp
}
// tries to parse the URI so we can fail early
fn validate_uri(uri_str: Option<String>) -> anyhow::Result<Option<String>> {
	let Some(uri_str) = uri_str else {
		return Ok(uri_str);
	};
	let uri = http::Uri::try_from(&uri_str)?;
	if uri.scheme().is_none() {
		return Ok(Some("https://".to_owned() + &uri_str));
	}
	Ok(Some(uri_str))
}

/// Parse worker threads configuration, supporting both fixed numbers and percentages
fn parse_worker_threads() -> anyhow::Result<usize> {
	match parse::<String>("WORKER_THREADS")? {
		Some(value) => {
			if let Some(percent_str) = value.strip_suffix('%') {
				// Parse as percentage
				let percent: f64 = percent_str
					.parse()
					.map_err(|e| anyhow::anyhow!("invalid percentage: {}", e))?;

				if percent <= 0.0 || percent > 100.0 {
					anyhow::bail!("percentage must be between 0 and 100".to_string())
				}

				let cpu_count = get_cpu_count()?;
				// Round up, minimum of 1
				let threads = ((cpu_count as f64 * percent / 100.0).ceil() as usize).max(1);
				Ok(threads)
			} else {
				// Parse as fixed number
				value
					.parse::<usize>()
					.map_err(|e| anyhow::anyhow!("invalid number: {}", e))
			}
		},
		None => Ok(get_cpu_count()?),
	}
}

fn get_cpu_count() -> anyhow::Result<usize> {
	// Allow overriding the count with an env var. This can be used to pass the CPU limit on Kubernetes
	// from the downward API.
	// Note the downward API will return the total thread count ("logical cores") if no limit is set,
	// so it is really the same as num_cpus.
	// We allow num_cpus for cases its not set (not on Kubernetes, etc).
	match parse::<usize>("CPU_LIMIT")? {
		Some(limit) => Ok(limit),
		// This is *logical cores*
		None => Ok(num_cpus::get()),
	}
}
