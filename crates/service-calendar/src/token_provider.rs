use std::sync::Arc;

use async_trait::async_trait;
use calendarchy_core::{AccountId, GoogleOAuthConfig, Keyring};
use chrono::{Duration, Utc};
use tokio::sync::Mutex;

use crate::google_api::AccessTokenProvider;

/// Bridges the OAuth/keyring layer in `calendarchy-core` to the Calendar API client's
/// `AccessTokenProvider`: returns the cached access token if it's still valid,
/// otherwise refreshes it (DESIGN_SPEC.md §7) and writes the refreshed tokens back to
/// the keyring before returning.
pub struct KeyringAccessTokenProvider {
    account_id: AccountId,
    keyring: Arc<dyn Keyring>,
    oauth_config: GoogleOAuthConfig,
    // Serializes concurrent refreshes for the same account so two in-flight calls
    // don't both refresh (and both write back to the keyring) at once.
    refresh_lock: Mutex<()>,
}

impl KeyringAccessTokenProvider {
    pub fn new(account_id: AccountId, keyring: Arc<dyn Keyring>, oauth_config: GoogleOAuthConfig) -> Self {
        Self {
            account_id,
            keyring,
            oauth_config,
            refresh_lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl AccessTokenProvider for KeyringAccessTokenProvider {
    async fn access_token(&self) -> anyhow::Result<String> {
        let _guard = self.refresh_lock.lock().await;

        let current = self
            .keyring
            .load_tokens(self.account_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no OAuth tokens stored for account {:?}", self.account_id))?;

        // Refresh a little ahead of actual expiry so a slow request doesn't race it.
        if current.expires_at > Utc::now() + Duration::seconds(60) {
            return Ok(current.access_token);
        }

        let refreshed = calendarchy_core::oauth::refresh_access_token(&self.oauth_config, &current).await?;
        self.keyring.store_tokens(self.account_id, &refreshed).await?;
        Ok(refreshed.access_token)
    }
}
