pub mod anthropic;
pub mod catalog;
pub mod detect;
pub mod google;
pub mod keyring_store;
pub mod openai;
pub mod provider;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use anthropic::{AnthropicProvider, context_window};
pub use catalog::{ModelCatalog, ModelInfo};
pub use detect::{LMSTUDIO_PORT, OLLAMA_PORT, detect_providers};
pub use google::GoogleProvider;
pub use keyring_store::{delete_key, list_configured_providers, resolve_key_with_warning, set_key};
pub use openai::{
    DEEPSEEK_BASE_URL, GROQ_BASE_URL, LMSTUDIO_BASE_URL, NVIDIA_BASE_URL, OLLAMA_BASE_URL,
    OPENAI_BASE_URL, OpenAiCompatProvider,
};
pub use provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tokens_is_nonzero() {
        let provider = AnthropicProvider::new();
        assert!(provider.count_tokens("claude-opus-4-8", "hello world, four tokens") >= 1);
    }

    #[test]
    fn vendored_catalog_has_anthropic_models() {
        let cat = ModelCatalog::from_vendored();
        assert!(cat.get("anthropic", "claude-opus-4-8").is_some());
    }
}
