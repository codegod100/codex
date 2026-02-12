//! Registry of model providers supported by Codex.
//!
//! Providers can be defined in two places:
//!   1. Built-in defaults compiled into the binary so Codex works out-of-the-box.
//!   2. User-defined entries inside `~/.codex/config.toml` under the `model_providers`
//!      key. These override or extend the defaults at runtime.

use crate::auth::AuthMode;
use crate::auth::GITHUB_TOKEN_ENV_VAR;
use crate::auth::load_copilot_access_token_from_default_home;
use crate::error::EnvVarError;
use codex_api::Provider as ApiProvider;
use codex_api::provider::RetryConfig as ApiRetryConfig;
use http::HeaderMap;
use http::header::HeaderName;
use http::header::HeaderValue;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_STREAM_MAX_RETRIES: u64 = 5;
const DEFAULT_REQUEST_MAX_RETRIES: u64 = 4;
/// Hard cap for user-configured `stream_max_retries`.
const MAX_STREAM_MAX_RETRIES: u64 = 100;
/// Hard cap for user-configured `request_max_retries`.
const MAX_REQUEST_MAX_RETRIES: u64 = 100;

const OPENAI_PROVIDER_NAME: &str = "OpenAI";
const COPILOT_PROVIDER_NAME: &str = "GitHub Copilot";
const COPILOT_DEFAULT_BASE_URL: &str = "https://api.githubcopilot.com/v1";
pub(crate) const LEGACY_OLLAMA_CHAT_PROVIDER_ID: &str = "ollama-chat";
pub(crate) const OLLAMA_CHAT_PROVIDER_REMOVED_ERROR: &str = "`ollama-chat` is no longer supported.\nHow to fix: replace `ollama-chat` with `ollama` in `model_provider`, `oss_provider`, or `--local-provider`.\nMore info: https://github.com/openai/codex/discussions/7782";

#[derive(Clone, Copy)]
struct OpenAiCompatibleProviderDefinition {
    id: &'static str,
    name: &'static str,
    base_url_env_var: &'static str,
    default_base_url: &'static str,
    env_key: Option<&'static str>,
    env_key_instructions: Option<&'static str>,
    wire_api: WireApi,
    env_http_headers: &'static [(&'static str, &'static str)],
}

const BUILTIN_OPENAI_COMPATIBLE_PROVIDERS: [OpenAiCompatibleProviderDefinition; 1] = [
    OpenAiCompatibleProviderDefinition {
        id: COPILOT_PROVIDER_ID,
        name: COPILOT_PROVIDER_NAME,
        base_url_env_var: "GITHUB_COPILOT_BASE_URL",
        default_base_url: COPILOT_DEFAULT_BASE_URL,
        env_key: Some("GITHUB_TOKEN"),
        env_key_instructions: Some(
            "Run `codex login --copilot` or set GITHUB_TOKEN to a token with access to GitHub Copilot models.",
        ),
        wire_api: WireApi::Responses,
        env_http_headers: &[],
    },
];

/// Wire protocol that the provider speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    /// The Responses API exposed by OpenAI at `/v1/responses`.
    #[default]
    Responses,
    /// The legacy Chat Completions API exposed at `/v1/chat/completions`.
    ChatCompletions,
}

impl<'de> Deserialize<'de> for WireApi {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "responses" => Ok(Self::Responses),
            // Keep "chat" as a backwards-compatible alias.
            "chat" | "chat_completions" => Ok(Self::ChatCompletions),
            _ => Err(serde::de::Error::unknown_variant(
                &value,
                &["responses", "chat_completions"],
            )),
        }
    }
}

/// Serializable representation of a provider definition.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderInfo {
    /// Friendly display name.
    pub name: String,
    /// Base URL for the provider's OpenAI-compatible API.
    pub base_url: Option<String>,
    /// Environment variable that stores the user's API key for this provider.
    pub env_key: Option<String>,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub env_key_instructions: Option<String>,

    /// Value to use with `Authorization: Bearer <token>` header. Use of this
    /// config is discouraged in favor of `env_key` for security reasons, but
    /// this may be necessary when using this programmatically.
    pub experimental_bearer_token: Option<String>,

    /// Which wire protocol this provider expects.
    #[serde(default)]
    pub wire_api: WireApi,

    /// Optional model-prefix overrides for wire protocol selection within this provider.
    ///
    /// Keys are matched as prefixes against the selected model slug. The longest matching
    /// prefix wins. If no prefixes match, `wire_api` is used.
    #[serde(default)]
    pub wire_api_by_model: Option<HashMap<String, WireApi>>,

    /// Optional query parameters to append to the base URL.
    pub query_params: Option<HashMap<String, String>>,

    /// Additional HTTP headers to include in requests to this provider where
    /// the (key, value) pairs are the header name and value.
    pub http_headers: Option<HashMap<String, String>>,

    /// Optional HTTP headers to include in requests to this provider where the
    /// (key, value) pairs are the header name and _environment variable_ whose
    /// value should be used. If the environment variable is not set, or the
    /// value is empty, the header will not be included in the request.
    pub env_http_headers: Option<HashMap<String, String>>,

    /// Maximum number of times to retry a failed HTTP request to this provider.
    pub request_max_retries: Option<u64>,

    /// Number of times to retry reconnecting a dropped streaming response before failing.
    pub stream_max_retries: Option<u64>,

    /// Idle timeout (in milliseconds) to wait for activity on a streaming response before treating
    /// the connection as lost.
    pub stream_idle_timeout_ms: Option<u64>,

    /// Does this provider require an OpenAI API Key or ChatGPT login token? If true,
    /// user is presented with login screen on first run, and login preference and token/key
    /// are stored in auth.json. If false (which is the default), login screen is skipped,
    /// and API key (if needed) comes from the "env_key" environment variable.
    #[serde(default)]
    pub requires_openai_auth: bool,

    /// Whether this provider supports the Responses API WebSocket transport.
    #[serde(default)]
    pub supports_websockets: bool,

    /// Whether streaming responses are enabled for this provider.
    ///
    /// Defaults to true. Set to false to force non-streaming requests for
    /// providers with unstable SSE support.
    pub stream: Option<bool>,
}

impl ModelProviderInfo {
    fn env_http_headers_from_pairs(pairs: &[(&str, &str)]) -> Option<HashMap<String, String>> {
        if pairs.is_empty() {
            None
        } else {
            Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }

    fn base_url_from_env_or_default(
        base_url_env_var: &str,
        default_base_url: &str,
    ) -> Option<String> {
        std::env::var(base_url_env_var)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| Some(default_base_url.to_string()))
    }

    fn create_openai_compatible_provider(
        name: &str,
        base_url_env_var: &str,
        default_base_url: &str,
        env_key: Option<&str>,
        env_key_instructions: Option<&str>,
        env_http_headers: Option<HashMap<String, String>>,
    ) -> ModelProviderInfo {
        ModelProviderInfo {
            name: name.into(),
            base_url: Self::base_url_from_env_or_default(base_url_env_var, default_base_url),
            env_key: env_key.map(str::to_string),
            env_key_instructions: env_key_instructions.map(str::to_string),
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: None,
            query_params: None,
            http_headers: None,
            env_http_headers,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            stream: None,
        }
    }

    fn create_from_openai_compatible_definition(
        definition: OpenAiCompatibleProviderDefinition,
    ) -> ModelProviderInfo {
        let mut provider = Self::create_openai_compatible_provider(
            definition.name,
            definition.base_url_env_var,
            definition.default_base_url,
            definition.env_key,
            definition.env_key_instructions,
            Self::env_http_headers_from_pairs(definition.env_http_headers),
        );
        provider.wire_api = definition.wire_api;
        provider
    }

    fn build_header_map(&self) -> crate::error::Result<HeaderMap> {
        let capacity = self.http_headers.as_ref().map_or(0, HashMap::len)
            + self.env_http_headers.as_ref().map_or(0, HashMap::len);
        let mut headers = HeaderMap::with_capacity(capacity);
        if let Some(extra) = &self.http_headers {
            for (k, v) in extra {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
                    headers.insert(name, value);
                }
            }
        }

        if let Some(env_headers) = &self.env_http_headers {
            for (header, env_var) in env_headers {
                if let Ok(val) = std::env::var(env_var)
                    && !val.trim().is_empty()
                    && let (Ok(name), Ok(value)) =
                        (HeaderName::try_from(header), HeaderValue::try_from(val))
                {
                    headers.insert(name, value);
                }
            }
        }

        Ok(headers)
    }

    pub(crate) fn to_api_provider(
        &self,
        auth_mode: Option<AuthMode>,
    ) -> crate::error::Result<ApiProvider> {
        let default_base_url = if matches!(auth_mode, Some(AuthMode::Chatgpt)) {
            "https://chatgpt.com/backend-api/codex"
        } else {
            "https://api.openai.com/v1"
        };
        let base_url = self
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url.to_string());

        let headers = self.build_header_map()?;
        let retry = ApiRetryConfig {
            max_attempts: self.request_max_retries(),
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: true,
            retry_transport: true,
        };

        Ok(ApiProvider {
            name: self.name.clone(),
            base_url,
            query_params: self.query_params.clone(),
            headers,
            retry,
            stream_idle_timeout: self.stream_idle_timeout(),
        })
    }

    /// If `env_key` is Some, returns the API key for this provider if present
    /// (and non-empty) in the environment. If `env_key` is required but
    /// cannot be found, returns an error.
    pub fn api_key(&self) -> crate::error::Result<Option<String>> {
        match &self.env_key {
            Some(env_key) => {
                let api_key = if let Ok(value) = std::env::var(env_key) {
                    if value.trim().is_empty() {
                        None
                    } else {
                        Some(value)
                    }
                } else if env_key == GITHUB_TOKEN_ENV_VAR {
                    load_copilot_access_token_from_default_home().ok().flatten()
                } else {
                    None
                }
                .ok_or_else(|| {
                        crate::error::CodexErr::EnvVar(EnvVarError {
                            var: env_key.clone(),
                            instructions: self.env_key_instructions.clone(),
                        })
                    })?;
                Ok(Some(api_key))
            }
            None => Ok(None),
        }
    }

    /// Effective maximum number of request retries for this provider.
    pub fn request_max_retries(&self) -> u64 {
        self.request_max_retries
            .unwrap_or(DEFAULT_REQUEST_MAX_RETRIES)
            .min(MAX_REQUEST_MAX_RETRIES)
    }

    /// Effective maximum number of stream reconnection attempts for this provider.
    pub fn stream_max_retries(&self) -> u64 {
        self.stream_max_retries
            .unwrap_or(DEFAULT_STREAM_MAX_RETRIES)
            .min(MAX_STREAM_MAX_RETRIES)
    }

    /// Effective idle timeout for streaming responses.
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS))
    }

    pub fn stream_enabled(&self) -> bool {
        self.stream.unwrap_or(true)
    }

    pub fn wire_api_for_model(&self, model: &str) -> WireApi {
        self.wire_api_by_model
            .as_ref()
            .and_then(|wire_api_by_model| {
                wire_api_by_model
                    .iter()
                    .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
                    .max_by_key(|(prefix, _)| prefix.len())
                    .map(|(_, wire_api)| *wire_api)
            })
            .unwrap_or(self.wire_api)
    }

    pub fn create_openai_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: OPENAI_PROVIDER_NAME.into(),
            // Allow users to override the default OpenAI endpoint by
            // exporting `OPENAI_BASE_URL`. This is useful when pointing
            // Codex at a proxy, mock server, or Azure-style deployment
            // without requiring a full TOML override for the built-in
            // OpenAI provider.
            base_url: std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: None,
            query_params: None,
            http_headers: Some(
                [("version".to_string(), env!("CARGO_PKG_VERSION").to_string())]
                    .into_iter()
                    .collect(),
            ),
            env_http_headers: Some(
                [
                    (
                        "OpenAI-Organization".to_string(),
                        "OPENAI_ORGANIZATION".to_string(),
                    ),
                    ("OpenAI-Project".to_string(), "OPENAI_PROJECT".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            // Use global defaults for retry/timeout unless overridden in config.toml.
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: true,
            supports_websockets: true,
            stream: None,
        }
    }

    pub fn is_openai(&self) -> bool {
        self.name == OPENAI_PROVIDER_NAME
    }
}

pub const DEFAULT_LMSTUDIO_PORT: u16 = 1234;
pub const DEFAULT_OLLAMA_PORT: u16 = 11434;

pub const LMSTUDIO_OSS_PROVIDER_ID: &str = "lmstudio";
pub const OLLAMA_OSS_PROVIDER_ID: &str = "ollama";
pub const COPILOT_PROVIDER_ID: &str = "copilot";

/// Built-in default provider list.
pub fn built_in_model_providers() -> HashMap<String, ModelProviderInfo> {
    // We intentionally keep built-ins small and practical: first-party defaults
    // plus providers needed for common local and Copilot-backed setups.
    // Users can add any other providers in `model_providers`.
    let mut providers = HashMap::from([(
        "openai".to_string(),
        ModelProviderInfo::create_openai_provider(),
    )]);
    providers.extend(
        BUILTIN_OPENAI_COMPATIBLE_PROVIDERS
            .into_iter()
            .map(|definition| {
                (
                    definition.id.to_string(),
                    ModelProviderInfo::create_from_openai_compatible_definition(definition),
                )
            }),
    );
    providers.insert(
        OLLAMA_OSS_PROVIDER_ID.to_string(),
        create_oss_provider(DEFAULT_OLLAMA_PORT, WireApi::Responses),
    );
    providers.insert(
        LMSTUDIO_OSS_PROVIDER_ID.to_string(),
        create_oss_provider(DEFAULT_LMSTUDIO_PORT, WireApi::Responses),
    );
    providers
}

pub fn create_oss_provider(default_provider_port: u16, wire_api: WireApi) -> ModelProviderInfo {
    // These CODEX_OSS_ environment variables are experimental: we may
    // switch to reading values from config.toml instead.
    let default_codex_oss_base_url = format!(
        "http://localhost:{codex_oss_port}/v1",
        codex_oss_port = std::env::var("CODEX_OSS_PORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(default_provider_port)
    );

    let codex_oss_base_url = std::env::var("CODEX_OSS_BASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(default_codex_oss_base_url);
    create_oss_provider_with_base_url(&codex_oss_base_url, wire_api)
}

pub fn create_oss_provider_with_base_url(base_url: &str, wire_api: WireApi) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "gpt-oss".into(),
        base_url: Some(base_url.into()),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api,
        wire_api_by_model: None,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        stream: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_deserialize_ollama_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Ollama"
base_url = "http://localhost:11434/v1"
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Ollama".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: None,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            stream: None,
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_azure_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Azure"
base_url = "https://xxxxx.openai.azure.com/openai"
env_key = "AZURE_OPENAI_API_KEY"
query_params = { api-version = "2025-04-01-preview" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Azure".into(),
            base_url: Some("https://xxxxx.openai.azure.com/openai".into()),
            env_key: Some("AZURE_OPENAI_API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: None,
            query_params: Some(maplit::hashmap! {
                "api-version".to_string() => "2025-04-01-preview".to_string(),
            }),
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            stream: None,
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_example_model_provider_toml() {
        let azure_provider_toml = r#"
name = "Example"
base_url = "https://example.com"
env_key = "API_KEY"
http_headers = { "X-Example-Header" = "example-value" }
env_http_headers = { "X-Example-Env-Header" = "EXAMPLE_ENV_VAR" }
        "#;
        let expected_provider = ModelProviderInfo {
            name: "Example".into(),
            base_url: Some("https://example.com".into()),
            env_key: Some("API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: None,
            query_params: None,
            http_headers: Some(maplit::hashmap! {
                "X-Example-Header".to_string() => "example-value".to_string(),
            }),
            env_http_headers: Some(maplit::hashmap! {
                "X-Example-Env-Header".to_string() => "EXAMPLE_ENV_VAR".to_string(),
            }),
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            stream: None,
        };

        let provider: ModelProviderInfo = toml::from_str(azure_provider_toml).unwrap();
        assert_eq!(expected_provider, provider);
    }

    #[test]
    fn test_deserialize_chat_wire_api_alias() {
        let provider_toml = r#"
name = "OpenAI using Chat Completions"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "chat"
        "#;

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(provider.wire_api, WireApi::ChatCompletions);
    }

    #[test]
    fn test_deserialize_chat_completions_wire_api() {
        let provider_toml = r#"
name = "OpenAI using Chat Completions"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "chat_completions"
        "#;

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(provider.wire_api, WireApi::ChatCompletions);
    }

    #[test]
    fn test_deserialize_wire_api_by_model() {
        let provider_toml = r#"
name = "OpenAI compatible"
base_url = "https://example.com/v1"
env_key = "EXAMPLE_API_KEY"
wire_api = "responses"
wire_api_by_model = { "gpt-5" = "responses", "claude-" = "chat_completions" }
        "#;

        let provider: ModelProviderInfo = toml::from_str(provider_toml).unwrap();
        assert_eq!(
            provider.wire_api_by_model,
            Some(maplit::hashmap! {
                "gpt-5".to_string() => WireApi::Responses,
                "claude-".to_string() => WireApi::ChatCompletions,
            })
        );
    }

    #[test]
    fn test_wire_api_for_model_uses_longest_prefix_match() {
        let provider = ModelProviderInfo {
            name: "OpenAI compatible".into(),
            base_url: Some("https://example.com/v1".into()),
            env_key: Some("EXAMPLE_API_KEY".into()),
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            wire_api_by_model: Some(maplit::hashmap! {
                "claude-".to_string() => WireApi::ChatCompletions,
                "claude-3-".to_string() => WireApi::Responses,
            }),
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
            stream: None,
        };

        assert_eq!(
            provider.wire_api_for_model("claude-3-7-sonnet"),
            WireApi::Responses
        );
        assert_eq!(
            provider.wire_api_for_model("claude-opus"),
            WireApi::ChatCompletions
        );
        assert_eq!(provider.wire_api_for_model("gpt-5"), WireApi::Responses);
    }

    #[test]
    fn builtins_include_copilot_provider() {
        let providers = built_in_model_providers();
        let copilot = providers
            .get(COPILOT_PROVIDER_ID)
            .expect("copilot provider should exist");
        assert_eq!(copilot.name, COPILOT_PROVIDER_NAME);
        assert_eq!(
            copilot.base_url.as_deref(),
            Some("https://api.githubcopilot.com/v1")
        );
        assert_eq!(copilot.env_key.as_deref(), Some("GITHUB_TOKEN"));
        assert!(!copilot.requires_openai_auth);
    }
}
