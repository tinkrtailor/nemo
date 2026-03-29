use anyhow::Result;
use reqwest::Client;
use serde::de::DeserializeOwned;

/// HTTP client wrapper for communicating with the Nemo API server.
pub struct NemoClient {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl NemoClient {
    pub fn new(base_url: &str, api_key: Option<&str>, insecure: bool) -> Result<Self> {
        let client = Client::builder()
            .danger_accept_invalid_certs(insecure)
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(String::from),
        })
    }

    fn auth_header(&self) -> Option<String> {
        self.api_key.as_ref().map(|key| format!("Bearer {key}"))
    }

    pub async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.post(&url).json(body);

        if let Some(auth) = self.auth_header() {
            req = req.header("authorization", auth);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            anyhow::bail!("API error ({status}): {body}");
        }

        Ok(resp.json().await?)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.get(&url);

        if let Some(auth) = self.auth_header() {
            req = req.header("authorization", auth);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            anyhow::bail!("API error ({status}): {body}");
        }

        Ok(resp.json().await?)
    }

    pub async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.delete(&url);

        if let Some(auth) = self.auth_header() {
            req = req.header("authorization", auth);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            anyhow::bail!("API error ({status}): {body}");
        }

        Ok(resp.json().await?)
    }

    /// Register credentials with the control plane.
    pub async fn register_credentials(
        &self,
        engineer: &str,
        provider: &str,
        cred_content: &str,
        email: Option<&str>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "engineer": engineer,
            "provider": provider,
            "credential_ref": cred_content,
            "valid": true,
        });
        if let Some(e) = email {
            body["email"] = serde_json::json!(e);
        }
        let _: serde_json::Value = self.post("/credentials", &body).await?;
        Ok(())
    }

    /// Stream SSE events from a URL. Returns raw text lines.
    pub async fn get_stream(&self, path: &str) -> Result<reqwest::Response> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.client.get(&url);

        if let Some(auth) = self.auth_header() {
            req = req.header("authorization", auth);
        }

        let resp = req.send().await?;
        let status = resp.status();

        if !status.is_success() {
            let body = resp.text().await?;
            anyhow::bail!("API error ({status}): {body}");
        }

        Ok(resp)
    }
}
