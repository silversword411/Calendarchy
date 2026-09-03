use std::collections::HashMap;

use async_trait::async_trait;
use zbus::zvariant::Value;

/// How urgently a notification should be flagged to the OS notification daemon —
/// mirrors the freedesktop notification spec's `urgency` hint byte (0/1/2), which is
/// also exactly what Omarchy's own `omarchy-notification-send` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    fn as_byte(self) -> u8 {
        match self {
            Urgency::Low => 0,
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }
    }
}

/// One outgoing desktop notification. `glyph` is Omarchy's own `omarchy-glyph` hint —
/// a Nerd Font glyph shown on the notification card by Omarchy's Quickshell shell,
/// the same hint `/usr/bin/omarchy-notification-send` sets — `None` omits it, which
/// every non-Omarchy freedesktop notification daemon already treats as "no icon".
pub struct NotifyRequest {
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub glyph: Option<String>,
}

/// Sends event-reminder notifications to the desktop's `org.freedesktop.Notifications`
/// D-Bus service (DESIGN_SPEC.md §13a) — Quickshell owns this name on Omarchy, but the
/// interface is the freedesktop standard, not Omarchy-specific, so this works on any
/// compliant notification daemon. `DbusNotifier` is the real backend; a fake can be
/// swapped in for tests the same way `InMemoryKeyring` stands in for `SecretServiceKeyring`.
#[async_trait]
pub trait SystemNotifier: Send + Sync {
    async fn notify(&self, req: NotifyRequest) -> anyhow::Result<u32>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

/// Real `SystemNotifier`, over the session D-Bus. Connects fresh on every call rather
/// than caching a connection — reminder notifications are infrequent (at most one poll
/// tick's worth every ~20s, DESIGN_SPEC.md §13), so the extra handshake cost is
/// negligible and this sidesteps having to detect/recover a dropped/stale connection.
pub struct DbusNotifier;

impl DbusNotifier {
    pub fn new() -> Self {
        Self
    }
}

impl Default for DbusNotifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SystemNotifier for DbusNotifier {
    async fn notify(&self, req: NotifyRequest) -> anyhow::Result<u32> {
        let connection = zbus::Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;

        // No actions are wired: Calendarchy doesn't listen for `ActionInvoked`, and
        // Omarchy's own notification tooling makes the same choice for the same
        // reason — action buttons with nothing listening are worse than none. All
        // interactive snooze/dismiss lives in Calendarchy's own floating dialog.
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("urgency", Value::U8(req.urgency.as_byte()));
        if let Some(glyph) = &req.glyph {
            hints.insert("omarchy-glyph", Value::from(glyph.as_str()));
        }

        let id = proxy
            .notify(&req.app_name, 0, "", &req.summary, &req.body, &[], hints, -1)
            .await?;
        Ok(id)
    }
}
