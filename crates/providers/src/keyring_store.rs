//! OS keychain integration for API keys (Phase 5, Step 5.6).
//!
//! Wraps the `keyring` crate with a stable service namespace and env fallback.

use crate::provider::{ProviderError, resolve_api_key};
use tracing::warn;

const SERVICE: &str = "private-code";

/// Store an API key for a provider in the OS keychain.
pub fn set_key(provider_name: &str, key: &str) -> Result<(), ProviderError> {
    keyring::Entry::new(SERVICE, provider_name)
        .map_err(|e| ProviderError::Other(format!("keyring entry: {e}")))?
        .set_password(key)
        .map_err(|e| ProviderError::Other(format!("keyring set: {e}")))
}

/// Remove a stored API key.
pub fn delete_key(provider_name: &str) -> Result<(), ProviderError> {
    keyring::Entry::new(SERVICE, provider_name)
        .map_err(|e| ProviderError::Other(format!("keyring entry: {e}")))?
        .delete_password()
        .map_err(|e| ProviderError::Other(format!("keyring delete: {e}")))
}

/// List provider names that have keys configured (keyring or env). Never returns key values.
pub fn list_configured_providers(candidates: &[&str]) -> Vec<String> {
    candidates
        .iter()
        .filter(|name| resolve_api_key(name).is_ok())
        .map(|s| (*s).to_string())
        .collect()
}

/// Resolve a key with a warning when falling back to environment variables.
pub fn resolve_key_with_warning(provider_name: &str) -> Result<String, ProviderError> {
    if let Ok(entry) = keyring::Entry::new(SERVICE, provider_name)
        && let Ok(key) = entry.get_password()
    {
        return Ok(key);
    }

    let env_var = format!("{}_API_KEY", provider_name.to_uppercase());
    if let Ok(key) = std::env::var(&env_var) {
        warn!(
            "Using {env_var} env fallback for provider '{provider_name}' — prefer `private-code auth set {provider_name}`"
        );
        return Ok(key);
    }

    Err(ProviderError::ApiKeyNotFound(provider_name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_never_panics_on_empty_candidates() {
        assert!(list_configured_providers(&[]).is_empty());
    }
}
