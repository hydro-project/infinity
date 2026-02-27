use async_trait::async_trait;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;
use infinity_agent_core::traits::HttpClient;
use std::time::SystemTime;

/// HTTP client for invoking Lambda Function URLs with SigV4 IAM auth.
#[derive(Clone)]
pub struct RapHttpClient {
    http_client: reqwest::Client,
    credentials_provider: SharedCredentialsProvider,
    region: String,
}

#[derive(Debug)]
pub struct HttpError(String);
impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for HttpError {}

impl RapHttpClient {
    pub fn new(config: &aws_config::SdkConfig) -> Self {
        let http_client = reqwest::Client::new();
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

    /// Sign a request with SigV4 and return the headers to add.
    async fn sign_request<'a>(
        &self,
        method: &'a str,
        url: &'a str,
        headers: impl Iterator<Item = (&'a str, &'a str)>,
        body: SignableBody<'a>,
    ) -> Result<Vec<(String, String)>, HttpError> {
        let creds = self
            .credentials_provider
            .provide_credentials()
            .await
            .map_err(|e| HttpError(e.to_string()))?;
        let identity = Identity::new(creds, None);

        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::Headers;

        let signing_params = SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("lambda")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .map_err(|e| HttpError(e.to_string()))?;

        let signable_request = SignableRequest::new(method, url, headers, body)
            .map_err(|e| HttpError(e.to_string()))?;

        let (signing_instructions, _) = sign(signable_request, &signing_params.into())
            .map_err(|e| HttpError(e.to_string()))?
            .into_parts();

        let signed_headers: Vec<(String, String)> = signing_instructions
            .headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        Ok(signed_headers)
    }
}

#[async_trait]
impl HttpClient for RapHttpClient {
    type Error = HttpError;

    async fn post(&self, url: &str, body: &str) -> Result<u16, HttpError> {
        let parsed = url::Url::parse(url).map_err(|e| HttpError(e.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or(HttpError("missing host".into()))?
            .to_string();

        let signed_headers = self
            .sign_request(
                "POST",
                url,
                std::iter::once(("host", host.as_str()))
                    .chain(std::iter::once(("content-type", "application/json"))),
                SignableBody::Bytes(body.as_bytes()),
            )
            .await?;

        let mut request = self
            .http_client
            .post(url)
            .header("host", &host)
            .header("content-type", "application/json");

        for (name, value) in &signed_headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| HttpError(e.to_string()))?;

        Ok(response.status().as_u16())
    }

    async fn get(&self, url: &str) -> Result<(u16, Vec<u8>), HttpError> {
        let parsed = url::Url::parse(url).map_err(|e| HttpError(e.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or(HttpError("missing host".into()))?
            .to_string();

        let signed_headers = self
            .sign_request(
                "GET",
                url,
                std::iter::once(("host", host.as_str()))
                    .chain(std::iter::once(("accept", "application/json"))),
                SignableBody::empty(),
            )
            .await?;

        let mut request = self
            .http_client
            .get(url)
            .header("host", &host)
            .header("accept", "application/json");

        for (name, value) in &signed_headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request.send().await.map_err(|e| HttpError(e.to_string()))?;

        let status = response.status().as_u16();
        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| HttpError(e.to_string()))?
            .to_vec();

        Ok((status, body_bytes))
    }
}
