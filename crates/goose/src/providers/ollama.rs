use super::base::{ConfigKey, Provider, ProviderMetadata, ProviderUsage, Usage};
use super::errors::ProviderError;
use super::utils::{get_model, handle_response_openai_compat};
use crate::impl_provider_default;
use crate::message::Message;
use crate::model::ModelConfig;
use crate::providers::formats::openai::{create_request, get_usage, response_to_message};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use rmcp::model::Tool;
use serde_json::Value;
use std::time::Duration;
use url::Url;

pub const OLLAMA_HOST: &str = "localhost";
pub const OLLAMA_TIMEOUT: u64 = 600; // seconds
pub const OLLAMA_DEFAULT_PORT: u16 = 11434;
pub const OLLAMA_DEFAULT_MODEL: &str = "qwen2.5";
// Ollama can run many models, we only provide the default
pub const OLLAMA_KNOWN_MODELS: &[&str] = &[OLLAMA_DEFAULT_MODEL];
pub const OLLAMA_DOC_URL: &str = "https://ollama.com/library";

#[derive(serde::Serialize)]
pub struct OllamaProvider {
    #[serde(skip)]
    client: Client,
    host: String,
    model: ModelConfig,
}

impl_provider_default!(OllamaProvider);

impl OllamaProvider {
    pub fn from_env(model: ModelConfig) -> Result<Self> {
        let config = crate::config::Config::global();
        let host: String = config
            .get_param("OLLAMA_HOST")
            .unwrap_or_else(|_| OLLAMA_HOST.to_string());

        let timeout: Duration =
            Duration::from_secs(config.get_param("OLLAMA_TIMEOUT").unwrap_or(OLLAMA_TIMEOUT));

        let client = Client::builder().timeout(timeout).build()?;

        Ok(Self {
            client,
            host,
            model,
        })
    }

    /// Get the base URL for Ollama API calls
    fn get_base_url(&self) -> Result<Url, ProviderError> {
        // OLLAMA_HOST is sometimes just the 'host' or 'host:port' without a scheme
        let base = if self.host.starts_with("http://") || self.host.starts_with("https://") {
            &self.host
        } else {
            &format!("http://{}", self.host)
        };

        let mut base_url = Url::parse(base)
            .map_err(|e| ProviderError::RequestFailed(format!("Invalid base URL: {e}")))?;

        // Set the default port if missing
        // Don't add default port if:
        // 1. URL explicitly ends with standard ports (:80 or :443)
        // 2. URL uses HTTPS (which implicitly uses port 443)
        let explicit_default_port = self.host.ends_with(":80") || self.host.ends_with(":443");
        let is_https = base_url.scheme() == "https";

        if base_url.port().is_none() && !explicit_default_port && !is_https {
            base_url.set_port(Some(OLLAMA_DEFAULT_PORT)).map_err(|_| {
                ProviderError::RequestFailed("Failed to set default port".to_string())
            })?;
        }

        Ok(base_url)
    }

    async fn post(&self, payload: &Value) -> Result<Value, ProviderError> {
        // TODO: remove this later when the UI handles provider config refresh
        let base_url = self.get_base_url()?;

        let url = base_url.join("v1/chat/completions").map_err(|e| {
            ProviderError::RequestFailed(format!("Failed to construct endpoint URL: {e}"))
        })?;

        let response = self.client.post(url).json(payload).send().await?;

        handle_response_openai_compat(response).await
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata::new(
            "ollama",
            "Ollama",
            "Local open source models",
            OLLAMA_DEFAULT_MODEL,
            OLLAMA_KNOWN_MODELS.to_vec(),
            OLLAMA_DOC_URL,
            vec![
                ConfigKey::new("OLLAMA_HOST", true, false, Some(OLLAMA_HOST)),
                ConfigKey::new(
                    "OLLAMA_TIMEOUT",
                    false,
                    false,
                    Some(&(OLLAMA_TIMEOUT.to_string())),
                ),
            ],
        )
    }

    fn get_model_config(&self) -> ModelConfig {
        self.model.clone()
    }

    #[tracing::instrument(
        skip(self, system, messages, tools),
        fields(model_config, input, output, input_tokens, output_tokens, total_tokens)
    )]
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage), ProviderError> {
        let config = crate::config::Config::global();
        let goose_mode = config.get_param("GOOSE_MODE").unwrap_or("auto".to_string());
        let filtered_tools = if goose_mode == "chat" { &[] } else { tools };

        let payload = create_request(
            &self.model,
            system,
            messages,
            filtered_tools,
            &super::utils::ImageFormat::OpenAi,
        )?;
        let response = self.post(&payload).await?;
        let message = response_to_message(&response)?;

        let usage = response.get("usage").map(get_usage).unwrap_or_else(|| {
            tracing::debug!("Failed to get usage data");
            Usage::default()
        });
        let model = get_model(&response);
        super::utils::emit_debug_trace(&self.model, &payload, &response, &usage);
        Ok((message, ProviderUsage::new(model, usage)))
    }
}
