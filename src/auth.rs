use anyhow::Context;
use base64::Engine as _;
use codex_core::config::ConfigBuilder;
use codex_login::AuthManager;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};

#[derive(Clone, Debug)]
pub struct Session {
    pub base_url: String,
    pub codex_home: std::path::PathBuf,
    pub bearer_token: String,
    pub account_id: Option<String>,
    pub user_agent: String,
}

impl Session {
    pub async fn load(
        base_url_override: Option<String>,
        codex_home_override: Option<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        // Load Codex config with optional explicit codex_home override.
        let mut cfg_builder = ConfigBuilder::default().cli_overrides(Vec::new());
        if let Some(home) = codex_home_override {
            cfg_builder = cfg_builder.codex_home(home);
        }
        let cfg = cfg_builder.build().await?;

        let base_url = base_url_override
            .or_else(|| std::env::var("CODEX_CLOUD_TASKS_BASE_URL").ok())
            .unwrap_or_else(|| cfg.chatgpt_base_url.clone());
        let base_url = normalize_base_url(&base_url);

        let codex_home = cfg.codex_home.clone();
        let auth_manager = AuthManager::new(
            codex_home.clone(),
            false,
            cfg.cli_auth_credentials_store_mode,
        );

        let auth = auth_manager
            .auth()
            .await
            .ok_or_else(|| anyhow::anyhow!("Not signed in. Run `codex login` and re-try."))?;

        let bearer_token = auth.get_token().context("Failed to load ChatGPT token")?;
        if bearer_token.trim().is_empty() {
            anyhow::bail!("Not signed in (empty token). Run `codex login` and re-try.");
        }

        let account_id = auth
            .get_account_id()
            .or_else(|| extract_chatgpt_account_id(&bearer_token));

        let user_agent = codex_core::default_client::get_codex_user_agent();

        Ok(Self {
            base_url,
            codex_home,
            bearer_token,
            account_id,
            user_agent,
        })
    }

    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent).unwrap_or(HeaderValue::from_static("cloudex")),
        );
        let v = format!("Bearer {}", self.bearer_token);
        if let Ok(hv) = HeaderValue::from_str(&v) {
            headers.insert(AUTHORIZATION, hv);
        }
        if let Some(acc) = &self.account_id
            && let Ok(name) = HeaderName::from_bytes(b"ChatGPT-Account-Id")
            && let Ok(hv) = HeaderValue::from_str(acc)
        {
            headers.insert(name, hv);
        }
        headers
    }

    pub fn cloud_client(&self) -> anyhow::Result<codex_cloud_tasks_client::HttpClient> {
        let mut c = codex_cloud_tasks_client::HttpClient::new(self.base_url.clone())?
            .with_user_agent(self.user_agent.clone())
            .with_bearer_token(self.bearer_token.clone());
        if let Some(acc) = &self.account_id {
            c = c.with_chatgpt_account_id(acc.clone());
        }
        Ok(c)
    }

    pub fn backend_client(&self) -> anyhow::Result<codex_backend_client::Client> {
        let mut c = codex_backend_client::Client::new(self.base_url.clone())?
            .with_user_agent(self.user_agent.clone())
            .with_bearer_token(self.bearer_token.clone());
        if let Some(acc) = &self.account_id {
            c = c.with_chatgpt_account_id(acc.clone());
        }
        Ok(c)
    }
}

/// Normalize the configured base URL to a canonical form used by the backend client.
/// - trims trailing '/'
/// - appends '/backend-api' for ChatGPT hosts when missing
pub fn normalize_base_url(input: &str) -> String {
    let mut base_url = input.to_string();
    while base_url.ends_with('/') {
        base_url.pop();
    }
    if (base_url.starts_with("https://chatgpt.com")
        || base_url.starts_with("https://chat.openai.com"))
        && !base_url.contains("/backend-api")
    {
        base_url = format!("{base_url}/backend-api");
    }
    base_url
}

/// Extract the ChatGPT account id from a JWT token, when present.
pub fn extract_chatgpt_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let (_h, payload_b64, _s) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return None,
    };
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    v.get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

/// Construct a browser-friendly task URL for the given backend base URL.
pub fn task_url(base_url: &str, task_id: &str) -> String {
    let normalized = normalize_base_url(base_url);
    if let Some(root) = normalized.strip_suffix("/backend-api") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if let Some(root) = normalized.strip_suffix("/api/codex") {
        return format!("{root}/codex/tasks/{task_id}");
    }
    if normalized.ends_with("/codex") {
        return format!("{normalized}/tasks/{task_id}");
    }
    format!("{normalized}/codex/tasks/{task_id}")
}
