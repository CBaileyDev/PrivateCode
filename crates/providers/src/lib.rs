pub mod anthropic;
pub mod provider;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use anthropic::{AnthropicProvider, context_window};
pub use provider::{ModelProvider, ProviderError, ProviderEvent, resolve_api_key};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_tokens_is_nonzero() {
        let provider = AnthropicProvider::new();
        assert!(provider.count_tokens("claude-opus-4-8", "hello world, four tokens") >= 1);
    }
}
