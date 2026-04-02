use std::time::SystemTime;

use async_trait::async_trait;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;

use sandbox_core::callback::CallbackClient;

/// SigV4-signing callback client for Lambda Function URLs.
pub struct SigV4CallbackClient {
    http_client: reqwest::Client,
    credentials_provider: SharedCredentialsProvider,
    region: String,
}

impl SigV4CallbackClient {
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        let credentials_provider = config
            .credentials_provider()
            .expect("credentials provider required");
        let region = config
            .region()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "us-east-1".to_owned());
        Self {
            http_client: reqwest::Client::new(),
            credentials_provider,
            region,
        }
    }
}

#[async_trait]
impl CallbackClient for SigV4CallbackClient {
    async fn post_json(
        &self,
        url: &str,
        body: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let parsed = url::Url::parse(url)?;
        let host = parsed
            .host_str()
            .ok_or("callback URL missing host")?
            .to_owned();

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

        let (signing_instructions, _) =
            sign(signable_request, &signing_params.into())?.into_parts();

        let mut req = self
            .http_client
            .post(url)
            .header("host", &host)
            .header("content-type", "application/json");

        for (name, value) in signing_instructions.headers() {
            req = req.header(name, value);
        }

        req.body(body.to_owned()).send().await?;

        Ok(())
    }
}
