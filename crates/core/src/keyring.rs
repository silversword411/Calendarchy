use std::collections::HashMap;
use std::sync::Mutex;

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
/// plaintext file (DESIGN_SPEC.md §13) — the real implementation talks to the
/// freedesktop Secret Service over D-Bus (the `oo7` crate), which Omarchy's Quickshell
/// shell still exposes. That D-Bus-backed impl lands with the OAuth flow (§7); this
/// trait lets the rest of core develop and test against it before that's wired up.
pub trait Keyring: Send + Sync {
    fn store_tokens(&self, account_id: AccountId, tokens: &OAuthTokens) -> anyhow::Result<()>;
    fn load_tokens(&self, account_id: AccountId) -> anyhow::Result<Option<OAuthTokens>>;
    fn delete_tokens(&self, account_id: AccountId) -> anyhow::Result<()>;
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

impl Keyring for InMemoryKeyring {
    fn store_tokens(&self, account_id: AccountId, tokens: &OAuthTokens) -> anyhow::Result<()> {
        self.tokens
            .lock()
            .expect("keyring mutex poisoned")
            .insert(account_id, tokens.clone());
        Ok(())
    }

    fn load_tokens(&self, account_id: AccountId) -> anyhow::Result<Option<OAuthTokens>> {
        Ok(self
            .tokens
            .lock()
            .expect("keyring mutex poisoned")
            .get(&account_id)
            .cloned())
    }

    fn delete_tokens(&self, account_id: AccountId) -> anyhow::Result<()> {
        self.tokens
            .lock()
            .expect("keyring mutex poisoned")
            .remove(&account_id);
        Ok(())
    }
}
