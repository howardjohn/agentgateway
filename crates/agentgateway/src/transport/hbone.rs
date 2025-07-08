use std::fmt::{Display, Formatter};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use std::{fmt, task};

use agent_core::prelude::Strng;
use agent_hbone::Key;
use anyhow::anyhow;
use async_trait::async_trait;
use http::Uri;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioIo;

use crate::control::caclient::CaClient;
use crate::proxy::ProxyError;
use crate::store::Stores;
use crate::transport::stream::{Extension, Socket};
use crate::types::discovery;
use crate::types::discovery::{Identity, NetworkAddress};

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct WorkloadKey {
	pub dst_id: Vec<Identity>,
	pub dst: SocketAddr,
}

impl Key for WorkloadKey {
	fn dest(&self) -> SocketAddr {
		self.dst
	}
}

impl Display for WorkloadKey {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{}[", self.dst,)?;
		for i in &self.dst_id {
			write!(f, "{i}")?;
		}
		write!(f, "]")
	}
}

#[async_trait]
impl agent_hbone::pool::CertificateFetcher<WorkloadKey> for CaClient {
	async fn fetch_certificate(
		&self,
		key: WorkloadKey,
	) -> anyhow::Result<Arc<rustls::client::ClientConfig>> {
		let id = self.get_identity().await?;
		let tls = id.hbone_mtls(key.dst_id)?;
		Ok(tls.config)
	}
}
