use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;
use lambda_runtime::Error;
use std::time::SystemTime;

/// HTTP client for invoking Lambda Function URLs with SigV4 IAM auth.
pub struct RapHttpClient {
    http_client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<hyper::body::Bytes>,
    >,
    credentials_provider: SharedCredentialsProvider,
    region: String,
}

impl RapHttpClient {
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("native TLS roots")
            .https_only()
            .enable_http1()
            .enable_http2()
            .build();

        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);

        let credentials_provider = config
            .credentials_provider()
            .expect("credentials provider required")
            .clone();

        let region = config
            .region()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "us-east-1".to_string());

        Self {
            http_client,
            credentials_provider,
            region,
        }
    }

    /// POST JSON to a Lambda Function URL, signed with SigV4.
    pub async fn post_signed(&self, url: &str, body: &str) -> Result<hyper::StatusCode, Error> {
        let parsed = url::Url::parse(url)?;
        let host = parsed.host_str().ok_or("missing host in URL")?.to_string();

        let creds = self.credentials_provider.provide_credentials().await?;
        let identity = Identity::new(creds, None);

        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("lambda")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()?;

        let signable_request = SignableRequest::new(
            "POST",
            url,
            std::iter::once(("host", host.as_str()))
                .chain(std::iter::once(("content-type", "application/json"))),
            SignableBody::Bytes(body.as_bytes()),
        )?;

        let (signing_instructions, _signature) =
            sign(signable_request, &signing_params.into())?.into_parts();

        let mut request = http::Request::builder()
            .method("POST")
            .uri(url)
            .header("host", &host)
            .header("content-type", "application/json");

        // Apply SigV4 headers
        for (name, value) in signing_instructions.headers() {
            request = request.header(name, value);
        }

        let request = request.body(http_body_util::Full::new(hyper::body::Bytes::from(
            body.to_string(),
        )))?;

        let response = self.http_client.request(request).await?;
        Ok(response.status())
    }
}
