use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::keyring::Keyring;
use crate::service::{ServiceKind, ServiceRegistry};
use crate::storage::Storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub i64);

/// The identity provider behind an account. Google is the only one implemented; the
/// field exists from the start so adding another provider later (CalDAV, iCloud, ...)
/// doesn't require reshaping this struct — see DESIGN_SPEC.md §18.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    Google,
}

impl Provider {
    fn as_str(&self) -> &'static str {
        match self {
            Provider::Google => "google",
        }
    }

    fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "google" => Ok(Provider::Google),
            other => Err(anyhow::anyhow!("unknown provider: {other}")),
        }
    }
}

/// A signed-in identity — deliberately just identity (provider, email, profile info).
/// What the account can *do* is a separate, per-account set of enabled `Service`s
/// (see DESIGN_SPEC.md §6); this struct knows nothing about calendars, contacts, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider: Provider,
    pub email: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Owns account identities and which services are enabled per account. Deliberately
/// has no calendar-specific (or contacts-specific, etc.) knowledge — that all lives in
/// `Service` implementations reached through the `ServiceRegistry`. See
/// DESIGN_SPEC.md §6 for why this split exists.
///
/// `Clone` is cheap (every field is an `Arc`-backed handle) and deliberate: the
/// Preferences window's "Sync now"/"Clear cache and resync" actions (§12) move a
/// clone into a background task rather than reaching back into the GTK-thread-bound
/// `App` model.
#[derive(Clone)]
pub struct AccountManager {
    storage: Storage,
    registry: Arc<ServiceRegistry>,
    keyring: Arc<dyn Keyring>,
}

impl AccountManager {
    pub fn new(storage: Storage, registry: Arc<ServiceRegistry>, keyring: Arc<dyn Keyring>) -> Self {
        Self {
            storage,
            registry,
            keyring,
        }
    }

    pub fn registry(&self) -> &Arc<ServiceRegistry> {
        &self.registry
    }

    pub fn keyring(&self) -> &Arc<dyn Keyring> {
        &self.keyring
    }

    /// Builds the `ServiceContext` a `Service` needs to act on behalf of one account.
    /// Used by callers driving `Service::on_enabled`/`sync` directly — e.g. right
    /// after `enable_service`, or from the per-(account, service) sync loop
    /// (DESIGN_SPEC.md §9).
    pub fn service_context(&self, account_id: AccountId) -> crate::service::ServiceContext {
        crate::service::ServiceContext {
            account_id,
            storage: self.storage.clone(),
            keyring: self.keyring.clone(),
        }
    }

    /// Register a new account identity. Does not enable any service — the caller
    /// (the "Add Google Account" flow, once §7's OAuth exchange completes) enables
    /// whichever services the user asked for via `enable_service`.
    pub fn add_account(
        &self,
        provider: Provider,
        email: &str,
        display_name: Option<&str>,
        avatar_url: Option<&str>,
    ) -> anyhow::Result<AccountId> {
        self.storage.with_conn(|conn| {
            conn.execute(
                "INSERT INTO accounts (provider, email, display_name, avatar_url) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![provider.as_str(), email, display_name, avatar_url],
            )?;
            Ok(AccountId(conn.last_insert_rowid()))
        })
    }

    pub fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        self.storage.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, provider, email, display_name, avatar_url FROM accounts ORDER BY id",
            )?;
            let rows = stmt.query_map([], |row| {
                let provider_str: String = row.get(1)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    provider_str,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?;

            let mut accounts = Vec::new();
            for row in rows {
                let (id, provider_str, email, display_name, avatar_url) = row?;
                accounts.push(Account {
                    id: AccountId(id),
                    provider: Provider::parse(&provider_str)
                        .map_err(|e| rusqlite::Error::InvalidColumnName(e.to_string()))?,
                    email,
                    display_name,
                    avatar_url,
                });
            }
            Ok(accounts)
        })
    }

    /// Remove an account entirely: every enabled service is told to delete its local
    /// data, the keyring entry is dropped, then the account row itself (cascading to
    /// `account_services`) is deleted. Google-side token revocation happens in the
    /// OAuth layer (§7), which calls this after a successful revoke.
    pub async fn remove_account(&self, account_id: AccountId) -> anyhow::Result<()> {
        for kind in self.enabled_services(account_id)? {
            self.disable_service(account_id, kind)?;
        }
        self.keyring.delete_tokens(account_id).await?;
        self.storage.with_conn(|conn| {
            conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id.0])?;
            Ok(())
        })
    }

    pub fn enabled_services(&self, account_id: AccountId) -> anyhow::Result<Vec<ServiceKind>> {
        self.storage.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT service_type FROM account_services WHERE account_id = ?1 AND enabled = 1",
            )?;
            let rows = stmt.query_map([account_id.0], |row| row.get::<_, String>(0))?;
            let mut kinds = Vec::new();
            for row in rows {
                if let Some(kind) = parse_service_kind(&row?) {
                    kinds.push(kind);
                }
            }
            Ok(kinds)
        })
    }

    /// Marks a service enabled for an account. The actual OAuth scope request
    /// (incremental authorization, §7) and the service's first-run fetch
    /// (`Service::on_enabled`) are the caller's responsibility — this just records
    /// the toggle state so `enabled_services` and the Accounts settings screen agree.
    pub fn enable_service(&self, account_id: AccountId, kind: ServiceKind) -> anyhow::Result<()> {
        self.storage.with_conn(|conn| {
            conn.execute(
                "INSERT INTO account_services (account_id, service_type, enabled) VALUES (?1, ?2, 1)
                 ON CONFLICT(account_id, service_type) DO UPDATE SET enabled = 1",
                rusqlite::params![account_id.0, kind.as_str()],
            )?;
            Ok(())
        })
    }

    /// Disables a service for an account: runs that service's `on_disabled` to delete
    /// its local data, then flips the toggle off. The account and its other enabled
    /// services are untouched.
    pub fn disable_service(&self, account_id: AccountId, kind: ServiceKind) -> anyhow::Result<()> {
        if let Some(service) = self.registry.get(kind) {
            let ctx = crate::service::ServiceContext {
                account_id,
                storage: self.storage.clone(),
                keyring: self.keyring.clone(),
            };
            service.on_disabled(&ctx)?;
        }
        self.storage.with_conn(|conn| {
            conn.execute(
                "UPDATE account_services SET enabled = 0 WHERE account_id = ?1 AND service_type = ?2",
                rusqlite::params![account_id.0, kind.as_str()],
            )?;
            Ok(())
        })
    }
}

fn parse_service_kind(s: &str) -> Option<ServiceKind> {
    match s {
        "calendar" => Some(ServiceKind::Calendar),
        "contacts" => Some(ServiceKind::Contacts),
        "notes" => Some(ServiceKind::Notes),
        "tasks" => Some(ServiceKind::Tasks),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyring::InMemoryKeyring;

    fn manager() -> AccountManager {
        let storage = Storage::open_in_memory().expect("open");
        let registry = Arc::new(ServiceRegistry::new());
        let keyring = Arc::new(InMemoryKeyring::new());
        AccountManager::new(storage, registry, keyring)
    }

    #[tokio::test]
    async fn add_list_and_remove_account() {
        let mgr = manager();
        let id = mgr
            .add_account(Provider::Google, "jane@gmail.com", Some("Jane"), None)
            .expect("add");

        let accounts = mgr.list_accounts().expect("list");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, id);
        assert_eq!(accounts[0].email, "jane@gmail.com");

        mgr.remove_account(id).await.expect("remove");
        assert!(mgr.list_accounts().expect("list").is_empty());
    }

    #[test]
    fn enable_and_disable_service_toggles_independently() {
        let mgr = manager();
        let id = mgr
            .add_account(Provider::Google, "jane@gmail.com", None, None)
            .expect("add");

        assert!(mgr.enabled_services(id).expect("list").is_empty());

        mgr.enable_service(id, ServiceKind::Calendar).expect("enable");
        assert_eq!(mgr.enabled_services(id).expect("list"), vec![ServiceKind::Calendar]);

        mgr.disable_service(id, ServiceKind::Calendar).expect("disable");
        assert!(mgr.enabled_services(id).expect("list").is_empty());
    }
}
