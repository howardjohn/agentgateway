use macro_rules_attribute::apply;
use once_cell::sync::Lazy;
use secrecy::{ExposeSecret, SecretString};
use tracing::trace;

use crate::http::Request;
use crate::http::jwt::Claims;
use crate::llm::bedrock::AwsRegion;
use crate::proxy::ProxyError;
use crate::serdes::deser_key_from_file;
use crate::*;

#[apply(schema!)]
#[serde(untagged)]
pub enum AwsAuth {
	/// Use explicit AWS credentials
	#[serde(rename_all = "camelCase")]
	ExplicitConfig {
		#[serde(serialize_with = "ser_redact")]
		#[cfg_attr(feature = "schema", schemars(with = "String"))]
		access_key_id: SecretString,
		#[serde(serialize_with = "ser_redact")]
		#[cfg_attr(feature = "schema", schemars(with = "String"))]
		secret_access_key: SecretString,
		region: String,
		#[serde(serialize_with = "ser_redact", skip_serializing_if = "Option::is_none")]
		#[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
		session_token: Option<SecretString>,
		// TODO: make service configurable (only bedrock for now)
	},
	/// Use implicit AWS authentication (environment variables, IAM roles, etc.)
	Implicit {},
}

#[apply(schema!)]
pub enum SimpleBackendAuth {
	Passthrough {},
	Key(
		#[cfg_attr(feature = "schema", schemars(with = "FileOrInline"))]
		#[serde(
			serialize_with = "ser_redact",
			deserialize_with = "deser_key_from_file"
		)]
		SecretString,
	),
}

impl From<SimpleBackendAuth> for BackendAuth {
	fn from(value: SimpleBackendAuth) -> Self {
		match value {
			SimpleBackendAuth::Passthrough {} => BackendAuth::Passthrough {},
			SimpleBackendAuth::Key(key) => BackendAuth::Key(key),
		}
	}
}

#[apply(schema!)]
pub enum BackendAuth {
	Passthrough {},
	Key(
		#[cfg_attr(feature = "schema", schemars(with = "FileOrInline"))]
		#[serde(
			serialize_with = "ser_redact",
			deserialize_with = "deser_key_from_file"
		)]
		SecretString,
	),
	#[serde(rename = "gcp")]
	Gcp {},
	#[serde(rename = "aws")]
	Aws(AwsAuth),
}

pub async fn apply_backend_auth(
	auth: Option<&BackendAuth>,
	req: &mut Request,
) -> Result<(), ProxyError> {
	let Some(auth) = auth else {
		return Ok(());
	};
	match auth {
		BackendAuth::Passthrough {} => {
			// They should have a JWT policy defined. That will strip the token. Here we add it back
			if let Some(claim) = req.extensions().get::<Claims>()
				&& let Ok(mut token) =
					http::HeaderValue::from_str(&format!("Bearer {}", claim.jwt.expose_secret()))
			{
				token.set_sensitive(true);
				req.headers_mut().insert(http::header::AUTHORIZATION, token);
			}
		},
		BackendAuth::Key(k) => {
			// TODO: is it always a Bearer?
			if let Ok(mut token) = http::HeaderValue::from_str(&format!("Bearer {}", k.expose_secret())) {
				token.set_sensitive(true);
				req.headers_mut().insert(http::header::AUTHORIZATION, token);
			}
		},
		BackendAuth::Gcp {} => {
			let token = gcp::get_token()
				.await
				.map_err(ProxyError::BackendAuthenticationFailed)?;
			req.headers_mut().insert(http::header::AUTHORIZATION, token);
		},
		BackendAuth::Aws(_) => {
			// We handle this in 'apply_late_backend_auth' since it must come at the end!
		},
	}
	Ok(())
}

pub async fn apply_late_backend_auth(
	auth: Option<&BackendAuth>,
	req: &mut Request,
) -> Result<(), ProxyError> {
	let Some(auth) = auth else {
		return Ok(());
	};
	match auth {
		BackendAuth::Passthrough {} => {},
		BackendAuth::Key(k) => {},
		BackendAuth::Gcp {} => {},
		BackendAuth::Aws(aws_auth) => {
			aws::sign_request(req, aws_auth)
				.await
				.map_err(ProxyError::BackendAuthenticationFailed)?;
		},
	};
	Ok(())
}

mod gcp {
	use anyhow::anyhow;
	use aws_config::{BehaviorVersion, SdkConfig};
	use google_cloud_auth::credentials::CacheableResource;
	use google_cloud_auth::errors::CredentialsError;
	use google_cloud_auth::{credentials, errors};
	use http::{HeaderMap, HeaderName, HeaderValue};
	use tokio::sync::OnceCell;
	use tracing::trace;

	static CREDS: OnceCell<credentials::Credentials> = OnceCell::const_new();
	async fn creds<'a>() -> anyhow::Result<&'a credentials::Credentials> {
		Ok(
			CREDS
				.get_or_try_init(|| async { credentials::Builder::default().build() })
				.await?,
		)
	}

	pub async fn get_token() -> anyhow::Result<HeaderValue> {
		let mut token = get_headers_from_cache(creds().await?.headers(http::Extensions::new()).await?)?;
		let mut hv = token
			.remove(http::header::AUTHORIZATION)
			.ok_or(anyhow!("no authorization header"))?;
		hv.set_sensitive(true);
		trace!("attached GCP token");
		Ok(hv)
	}
	// What a terrible API... the older versions of this crate were usable but pulled in legacy dependency
	pub fn get_headers_from_cache(
		headers: CacheableResource<HeaderMap>,
	) -> Result<HeaderMap, CredentialsError> {
		match headers {
			CacheableResource::New { data, .. } => Ok(data),
			CacheableResource::NotModified => Err(CredentialsError::from_msg(
				false,
				"Expecting headers to be present",
			)),
		}
	}
}

mod aws {
	use std::time::SystemTime;

	use aws_config::{BehaviorVersion, SdkConfig};
	use aws_credential_types::Credentials;
	use aws_credential_types::provider::ProvideCredentials;
	use aws_sigv4::http_request::{SignableBody, sign};
	use aws_sigv4::sign::v4::SigningParams;
	use http_body_util::BodyExt;
	use secrecy::ExposeSecret;
	use tokio::sync::OnceCell;

	use crate::http::auth::AwsAuth;
	use crate::llm::bedrock::AwsRegion;
	use crate::*;

	pub async fn sign_request(req: &mut http::Request, aws_auth: &AwsAuth) -> anyhow::Result<()> {
		let creds = load_credentials(aws_auth).await?.into();

		// Get the region based on auth mode
		let region = match aws_auth {
			AwsAuth::ExplicitConfig { region, .. } => region.clone(),
			AwsAuth::Implicit {} => {
				// Try to get region from request extensions first, then fall back to AWS config
				if let Some(aws_region) = req.extensions().get::<AwsRegion>() {
					aws_region.region.clone()
				} else {
					// Fall back to region from AWS config
					let config = sdk_config().await;
					config
						.region()
						.map(|r| r.as_ref().to_string())
						.ok_or_else(|| anyhow::anyhow!("No region found in AWS config or request extensions"))?
				}
			},
		};

		trace!("AWS signing with region: {}, service: bedrock", region);

		// Sign the request
		let signing_params = SigningParams::builder()
			.identity(&creds)
			.region(&region)
			.name("bedrock")
			.time(SystemTime::now())
			.settings(aws_sigv4::http_request::SigningSettings::default())
			.build()?
			.into();

		let orig_body = std::mem::take(req.body_mut());
		let body = orig_body.collect().await?.to_bytes();

		let signable_request = aws_sigv4::http_request::SignableRequest::new(
			req.method().as_str(),
			req.uri().to_string().replace("http://", "https://"),
			req
				.headers()
				.iter()
				.filter_map(|(k, v)| {
					std::str::from_utf8(v.as_bytes())
						.ok()
						.map(|v_str| (k.as_str(), v_str))
				})
				.filter(|(k, v)| k != &http::header::CONTENT_LENGTH),
			// SignableBody::UnsignedPayload,
			SignableBody::Bytes(body.as_ref()),
		)?;

		let (signature, _sig) = sign(signable_request, &signing_params)?.into_parts();
		signature.apply_to_request_http1x(req);

		req.headers_mut().insert(
			http::header::CONTENT_LENGTH,
			http::HeaderValue::from_str(&format!("{}", body.as_ref().len()))?,
		);
		*req.body_mut() = http::Body::from(body);

		trace!("signed AWS request");
		Ok(())
	}

	static SDK_CONFIG: OnceCell<SdkConfig> = OnceCell::const_new();
	async fn sdk_config<'a>() -> &'a SdkConfig {
		SDK_CONFIG
			.get_or_init(|| async { aws_config::load_defaults(BehaviorVersion::latest()).await })
			.await
	}

	async fn load_credentials(aws_auth: &AwsAuth) -> anyhow::Result<Credentials> {
		match aws_auth {
			AwsAuth::ExplicitConfig {
				access_key_id,
				secret_access_key,
				session_token,
				region: _,
			} => {
				// Use explicit credentials
				let mut builder = Credentials::builder()
					.access_key_id(access_key_id.expose_secret())
					.secret_access_key(secret_access_key.expose_secret())
					.provider_name("bedrock");

				if let Some(token) = session_token {
					builder = builder.session_token(token.expose_secret());
				}

				Ok(builder.build())
			},
			AwsAuth::Implicit {} => {
				// Load AWS configuration and credentials from environment/IAM
				let config = sdk_config().await;

				// Get credentials from the config
				// TODO this is not caching!!
				Ok(
					config
						.credentials_provider()
						.unwrap()
						.provide_credentials()
						.await?,
				)
			},
		}
	}
}
