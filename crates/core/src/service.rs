use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::Connection;

use crate::account::AccountId;
use crate::keyring::Keyring;
use crate::storage::Storage;

/// Identifies a pluggable capability an account can provide. New services are added
/// by extending this enum and providing a `Service` impl — see DESIGN_SPEC.md §6 for
/// why accounts (identity) and services (capabilities) are kept as separate concepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ServiceKind {
    Calendar,
    Contacts,
    Notes,
    Tasks,
}

impl ServiceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceKind::Calendar => "calendar",
            ServiceKind::Contacts => "contacts",
            ServiceKind::Notes => "notes",
            ServiceKind::Tasks => "tasks",
        }
    }
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a `Service` needs to act on behalf of one (account, service) pair.
/// `keyring` is here (rather than each service reaching for its own) because every
/// OAuth-based service needs the same account-scoped token lookup/refresh dance.
pub struct ServiceContext {
    pub account_id: AccountId,
    pub storage: Storage,
    pub keyring: Arc<dyn Keyring>,
}

/// A pluggable account capability (Calendar today; Contacts/Notes/Tasks are meant to
/// become new implementations of this trait, not changes to the Account Manager).
///
/// Each enabled (account, service) pair gets its own sync loop calling `sync()` on its
/// own schedule — see DESIGN_SPEC.md §9. Implementations own their local schema
/// (`migrate`) and their required OAuth scopes (`oauth_scopes`), which are only ever
/// requested for accounts where the service has actually been enabled.
#[async_trait]
pub trait Service: Send + Sync {
    fn kind(&self) -> ServiceKind;

    /// Shown as the toggle label on the Accounts settings screen.
    fn display_name(&self) -> &'static str;

    /// OAuth scopes this service needs, requested via incremental authorization
    /// only once the user turns the service on for a given account.
    fn oauth_scopes(&self) -> &'static [&'static str];

    /// False for services shown as a greyed-out "coming soon" toggle.
    fn is_available(&self) -> bool {
        true
    }

    /// Create/upgrade this service's own tables. Called once at startup for every
    /// registered service, independent of whether any account has it enabled yet.
    fn migrate(&self, conn: &Connection) -> anyhow::Result<()>;

    /// First-run fetch, called once right after the service is enabled for an account.
    async fn on_enabled(&self, ctx: &ServiceContext) -> anyhow::Result<()>;

    /// One sync pass (initial or incremental) for this (account, service) pair.
    async fn sync(&self, ctx: &ServiceContext) -> anyhow::Result<()>;

    /// Delete this service's local data for one account. The account and its other
    /// enabled services are left untouched.
    fn on_disabled(&self, ctx: &ServiceContext) -> anyhow::Result<()>;
}

/// The set of services the app knows about, independent of which accounts have
/// enabled which ones. `AccountManager` consults this to know what to show on the
/// Accounts settings screen and what to run migrations for at startup.
#[derive(Default)]
pub struct ServiceRegistry {
    services: Vec<Arc<dyn Service>>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, service: Arc<dyn Service>) {
        self.services.push(service);
    }

    pub fn get(&self, kind: ServiceKind) -> Option<&Arc<dyn Service>> {
        self.services.iter().find(|s| s.kind() == kind)
    }

    pub fn all(&self) -> &[Arc<dyn Service>] {
        &self.services
    }

    pub fn migrate_all(&self, conn: &Connection) -> anyhow::Result<()> {
        for service in &self.services {
            service.migrate(conn)?;
        }
        Ok(())
    }
}
