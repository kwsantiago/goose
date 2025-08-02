use super::errors::ProviderError;
use crate::impl_provider_default;
use crate::message::Message;
use crate::model::ModelConfig;
use crate::providers::base::{ConfigKey, Provider, ProviderMetadata, ProviderUsage, Usage};
use crate::providers::formats::openai::{create_request, get_usage, response_to_message};
use crate::providers::utils::get_model;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use rmcp::model::Tool;
use serde_json::Value;
use std::time::Duration;
use url::Url;

pub const GROQ_API_HOST: &str = "https://api.groq.com";
pub const GROQ_DEFAULT_MODEL: &str = "moonshotai/kimi-k2-instruct";
pub const GROQ_KNOWN_MODELS: &[&str] = &[
    "gemma2-9b-it",
    "llama-3.3-70b-versatile",
    "moonshotai/kimi-k2-instruct",
    "qwen/qwen3-32b",
];

pub const GROQ_DOC_URL: &str = "https://console.groq.com/docs/models";

#[derive(serde::Serialize)]
pub struct GroqProvider {
    #[serde(skip)]
    client: Client,
    host: String,
    api_key: String,
    model: ModelConfig,
}

impl_provider_default!(GroqProvider);

impl GroqProvider {
    pub fn from_env(model: ModelConfig) -> Result<Self> {
        let config = crate::config::Config::global();
        let api_key: String = config.get_secret("GROQ_API_KEY")?;
        let host: String = config
            .get_param("GROQ_HOST")
            .unwrap_or_else(|_| GROQ_API_HOST.to_string());

        let client = Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;

        Ok(Self {
            client,
            host,
            api_key,
            model,
        })
    }

    async fn post(&self, payload: &Value) -> anyhow::Result<Value, ProviderError> {
        let base_url = Url::parse(&self.host)
            .map_err(|e| ProviderError::RequestFailed(format!("Invalid base URL: {e}")))?;
        let url = base_url.join("openai/v1/chat/completions").map_err(|e| {
            ProviderError::RequestFailed(format!("Failed to construct endpoint URL: {e}"))
        })?;

        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(payload)
            .send()
            .await?;

        let status = response.status();
        let response_payload: Option<Value> = response.json().await.ok();
        let formatted_payload = format!("{:?}", response_payload);

        match status {
            StatusCode::OK => response_payload.ok_or_else( || ProviderError::RequestFailed("Response body is not valid JSON".to_string()) ),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(ProviderError::Authentication(format!("Authentication failed. Please ensure your API keys are valid and have the required permissions. \
                    Status: {}. Response: {:?}", status, response_payload)))
            }
            StatusCode::PAYLOAD_TOO_LARGE => {
                Err(ProviderError::ContextLengthExceeded(formatted_payload))
            }
            StatusCode::TOO_MANY_REQUESTS => {
                Err(ProviderError::RateLimitExceeded(formatted_payload))
            }
            StatusCode::INTERNAL_SERVER_ERROR | StatusCode::SERVICE_UNAVAILABLE => {
                Err(ProviderError::ServerError(formatted_payload))
            }
            _ => {
                let error_msg = format!("Provider request failed with status: {}. Payload: {:?}", status, response_payload);
                tracing::debug!(error_msg);
                Err(ProviderError::RequestFailed(error_msg))
            }
        }
    }
}

#[async_trait]
impl Provider for GroqProvider {
    fn metadata() -> ProviderMetadata {
        ProviderMetadata::new(
            "groq",
            "Groq",
            "Fast inference with Groq hardware",
            GROQ_DEFAULT_MODEL,
            GROQ_KNOWN_MODELS.to_vec(),
            GROQ_DOC_URL,
            vec![
                ConfigKey::new("GROQ_API_KEY", true, true, None),
                ConfigKey::new("GROQ_HOST", false, false, Some(GROQ_API_HOST)),
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
    ) -> anyhow::Result<(Message, ProviderUsage), ProviderError> {
        let payload = create_request(
            &self.model,
            system,
            messages,
            tools,
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

    /// Fetch supported models from Groq; returns Err on failure, Ok(None) if no models found
    async fn fetch_supported_models_async(&self) -> Result<Option<Vec<String>>, ProviderError> {
        // Construct the Groq models endpoint
        let base_url = url::Url::parse(&self.host)
            .map_err(|e| ProviderError::RequestFailed(format!("Invalid base URL: {}", e)))?;
        let url = base_url.join("openai/v1/models").map_err(|e| {
            ProviderError::RequestFailed(format!("Failed to construct endpoint URL: {}", e))
        })?;

        // Build the request with required headers
        let request = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json");

        // Send request
        let response = request.send().await?;
        let status = response.status();
        let payload: serde_json::Value = response.json().await.map_err(|_| {
            ProviderError::RequestFailed("Response body is not valid JSON".to_string())
        })?;

        // Check for error response from API
        if let Some(err_obj) = payload.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(ProviderError::Authentication(msg.to_string()));
        }

        // Extract model names
        if status == StatusCode::OK {
            let data = payload
                .get("data")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    ProviderError::UsageError("Missing or invalid `data` field in response".into())
                })?;

            let mut model_names: Vec<String> = data
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(String::from))
                .collect();
            model_names.sort();
            Ok(Some(model_names))
        } else {
            Err(ProviderError::RequestFailed(format!(
                "Groq API returned error status: {}. Payload: {:?}",
                status, payload
            )))
        }
    }
}
