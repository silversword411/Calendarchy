use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::account::AccountId;

/// OAuth tokens for one account. Stored per-account (not per-service) since a single
/// grant on an account can cover multiple enabled services' scopes — see
/// DESIGN_SPEC.md §7.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub granted_scopes: Vec<String>,
}

/// Secret storage for OAuth tokens. Tokens must never land in the SQLite DB or a
/// plaintext file (DESIGN_SPEC.md §13). `SecretServiceKeyring` is the real backend,
/// talking to the freedesktop Secret Service over D-Bus (which Omarchy's Quickshell
/// shell still exposes); `InMemoryKeyring` exists only for tests and early dev.
#[async_trait]
pub trait Keyring: Send + Sync {
    async fn store_tokens(&self, account_id: AccountId, tokens: &OAuthTokens) -> anyhow::Result<()>;
    async fn load_tokens(&self, account_id: AccountId) -> anyhow::Result<Option<OAuthTokens>>;
    async fn delete_tokens(&self, account_id: AccountId) -> anyhow::Result<()>;
}

/// In-memory `Keyring` for tests and early development. Not persistent across runs —
/// never use this outside of tests/dev before the Secret Service backend lands.
#[derive(Default)]
pub struct InMemoryKeyring {
    tokens: Mutex<HashMap<AccountId, OAuthTokens>>,
}

impl InMemoryKeyring {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Keyring for InMemoryKeyring {
    async fn store_tokens(&self, account_id: AccountId, tokens: &OAuthTokens) -> anyhow::Result<()> {
        self.tokens
            .lock()
            .expect("keyring mutex poisoned")
            .insert(account_id, tokens.clone());
        Ok(())
    }

    async fn load_tokens(&self, account_id: AccountId) -> anyhow::Result<Option<OAuthTokens>> {
        Ok(self
            .tokens
            .lock()
            .expect("keyring mutex poisoned")
            .get(&account_id)
            .cloned())
    }

    async fn delete_tokens(&self, account_id: AccountId) -> anyhow::Result<()> {
        self.tokens
            .lock()
            .expect("keyring mutex poisoned")
            .remove(&account_id);
        Ok(())
    }
}

/// The `xdg:schema` attribute value items are tagged with, so tools like Seahorse (and
/// our own `search_items` calls) can identify Calendarchy's entries in the keyring.
const SCHEMA: &str = "org.calendarchy.Calendarchy.Account";

/// Real `Keyring` backend: stores tokens as freedesktop Secret Service items over
/// D-Bus, via the `oo7` crate. `oo7::Keyring::new()` picks the Secret Service backend
/// automatically for a non-sandboxed native app like Calendarchy (it only falls back
/// to the portal/file backend when running inside a Flatpak sandbox).
pub struct SecretServiceKeyring {
    keyring: oo7::Keyring,
}

impl SecretServiceKeyring {
    pub async fn new() -> anyhow::Result<Self> {
        let keyring = oo7::Keyring::new().await?;
        Ok(Self { keyring })
    }

    fn attributes(account_id: AccountId) -> HashMap<String, String> {
        HashMap::from([
            ("xdg:schema".to_string(), SCHEMA.to_string()),
            ("account_id".to_string(), account_id.0.to_string()),
        ])
    }
}

#[async_trait]
impl Keyring for SecretServiceKeyring {
    async fn store_tokens(&self, account_id: AccountId, tokens: &OAuthTokens) -> anyhow::Result<()> {
        let secret = serde_json::to_string(tokens)?;
        let label = format!("Calendarchy account {}", account_id.0);
        self.keyring
            .create_item(&label, &Self::attributes(account_id), secret.as_str(), true)
            .await?;
        Ok(())
    }

    async fn load_tokens(&self, account_id: AccountId) -> anyhow::Result<Option<OAuthTokens>> {
        let items = self.keyring.search_items(&Self::attributes(account_id)).await?;
        let Some(item) = items.into_iter().next() else {
            return Ok(None);
        };
        let secret = item.secret().await?;
        let tokens = serde_json::from_slice(&secret)?;
        Ok(Some(tokens))
    }

    async fn delete_tokens(&self, account_id: AccountId) -> anyhow::Result<()> {
        // `delete` is a no-op if nothing matches, so removing an account that never
        // had tokens stored is not an error.
        self.keyring.delete(&Self::attributes(account_id)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_keyring_round_trips_tokens() {
        let keyring = InMemoryKeyring::new();
        let account_id = AccountId(1);
        let tokens = OAuthTokens {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: Utc::now(),
            granted_scopes: vec!["scope".into()],
        };

        assert!(keyring.load_tokens(account_id).await.unwrap().is_none());

        keyring.store_tokens(account_id, &tokens).await.unwrap();
        let loaded = keyring.load_tokens(account_id).await.unwrap().unwrap();
        assert_eq!(loaded.access_token, tokens.access_token);

        keyring.delete_tokens(account_id).await.unwrap();
        assert!(keyring.load_tokens(account_id).await.unwrap().is_none());
    }

    /// Exercises the real freedesktop Secret Service over D-Bus. Ignored by default
    /// since it needs a live session bus + unlocked keyring (not available in most CI
    /// sandboxes) — run explicitly with `cargo test -- --ignored` on a real desktop.
    #[tokio::test]
    #[ignore]
    async fn secret_service_keyring_round_trips_tokens() {
        let keyring = SecretServiceKeyring::new().await.expect("connect to Secret Service");
        // Fixed, clearly-scratch account id so a failed run's leftover item is easy to
        // spot and doesn't collide with any real account.
        let account_id = AccountId(-1);
        let tokens = OAuthTokens {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            expires_at: Utc::now(),
            granted_scopes: vec!["test-scope".into()],
        };

        keyring.delete_tokens(account_id).await.expect("pre-test cleanup");

        keyring.store_tokens(account_id, &tokens).await.expect("store");
        let loaded = keyring
            .load_tokens(account_id)
            .await
            .expect("load")
            .expect("tokens present after store");
        assert_eq!(loaded.access_token, tokens.access_token);
        assert_eq!(loaded.refresh_token, tokens.refresh_token);

        keyring.delete_tokens(account_id).await.expect("delete");
        assert!(keyring.load_tokens(account_id).await.expect("load after delete").is_none());
    }
}
