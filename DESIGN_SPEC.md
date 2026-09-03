# Calendarchy — Design Spec

A native calendar app for Omarchy, built in Rust. Multi-account Google Calendar sync, a real GTK4 window instead of a browser tab, and reminders that land in Omarchy's own notification system.

Status: v0.1 draft — pre-implementation.

---

## 1. Overview & Goals

Calendarchy is a native, offline-capable calendar client for Omarchy Linux. It aims to cover the everyday 90% of the Google Calendar PWA — view, create, and edit events across multiple Google accounts — while feeling like it belongs on an Omarchy desktop: fast startup, keyboard-driven, GTK4/libadwaita themed, and wired into the system notification center instead of a browser's notification permission prompt.

**Primary goals**

- View and manage events from **multiple Google accounts** side by side.
- Native GTK4 window with month / week / day / agenda views.
- **Easy account setup** — "Add Google Account" should be a two-click, browser-based OAuth flow, not a pasted API key or JSON file.
- Event reminders delivered through Omarchy's notification center, not a background browser tab.
- Usable **offline**, with changes syncing once back online.

**Secondary goals**

- Keyboard navigation throughout (matches Omarchy's keyboard-first philosophy).
- Respect the user's Omarchy theme (GTK4 accent color, light/dark mode) automatically.

## 2. Non-Goals (v1)

- **CalDAV, Outlook, or other providers.** Google Calendar only for account-based sync. The account/sync layer is written so this isn't foreclosed later, but it's not in scope now. (One narrow exception: read-only ICS-URL calendar subscriptions, §11 — not a provider integration, since it involves no account, no OAuth, and no write support.)
- **True real-time push.** See §9 — this would require a hosted relay service, which is explicitly deferred (§20).
- **Mobile companion app.**
- **Calendar administration** (creating shared calendars, managing ACLs/sharing permissions with other people). Users can still do this in the Google web UI; Calendarchy consumes calendars, it doesn't administer them.
- **Deep recurring-event editing UI** beyond what's needed for basic "this event / all events / this and following" edits (see §20 for why this is hard).
- **Google Workspace smart features** (auto-detected events from Gmail, smart chip suggestions, out-of-office/working-hours insights). These are computed by Google's own backend against broader Workspace scopes (Gmail, Drive) this app deliberately doesn't request, and aren't exposed through the Calendar API a pure-calendar client consumes (§12).
- **Booking pages.** Google Workspace's appointment-scheduling links are a scheduling product layered on top of Calendar, not a calendar-viewing/editing capability — no booking-page creation or management (§10).

## 3. Target Platform

- **Omarchy v4 "Quattro"** (Quickshell-based shell) — released Aug 2026. Quattro replaced the previous Waybar + Mako + Walker + SwayOSD stack with a single long-running Quickshell process (bar, launcher, notification center, OSDs, lock screen, polkit agent) exposed over an IPC-scriptable interface. This design targets Quattro, not the legacy stack.
- **Hyprland** as the compositor (Wayland).
- **Arch Linux**, distributed as an AUR package (§16).
- GTK4 + libadwaita for UI, so the app automatically inherits Omarchy's GTK4 theme, accent color, and light/dark mode — no custom theming layer needed.

## 4. Tech Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust | User requirement; also gives a small, fast, single-binary native app. |
| UI toolkit | `gtk4-rs` + `relm4` + `libadwaita` | Elm-style component architecture on top of real GTK4; automatic Omarchy theme inheritance; libadwaita gives native-feeling dialogs, headerbars, toasts. |
| Async runtime | `tokio` | Standard choice for the sync engine, HTTP client, and OAuth loopback server. |
| HTTP client | `reqwest` | Calendar API calls, OAuth token exchange. |
| OAuth | `oauth2` crate | Handles Authorization Code + PKCE flow mechanics. |
| Local storage | SQLite via `sqlx` (or `rusqlite`) | Simple, embedded, no server; well-understood for offline-first apps. |
| Secret storage | `oo7` (or `secret-service`) against the freedesktop Secret Service D-Bus API | OS keyring for OAuth tokens — Quickshell's shell still exposes the standard Secret Service, so this isn't Quattro-specific. |
| D-Bus / IPC | `zbus` | Freedesktop notifications (`org.freedesktop.Notifications`), single-instance enforcement, and (stretch) Quickshell IPC calls. |
| Packaging | Cargo + AUR `PKGBUILD` | Native to the Arch/Omarchy ecosystem. |

## 5. Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│  UI Layer (relm4 components)                            │
│  MainWindow, CalendarView, EventEditor, AccountsSettings │
└───────────────────────────┬───────────────────────────────┘
                             │ commands / view-models
┌───────────────────────────▼───────────────────────────────┐
│  App Core                                                 │
│  - Account Manager   (identity, OAuth flow, token lifecycle)│
│  - Service Registry  (Calendar service now; Contacts/Notes/│
│                        Tasks services register the same way)│
│  - Sync Engine(s)    (one per enabled account+service pair)│
│  - Notification Scheduler (reminders → D-Bus notifications)│
└───────────────┬────────────────────────┬───────────────────┘
                 │                        │
┌────────────────▼───────────┐  ┌─────────▼─────────────────┐
│  Google API Clients         │  │  Local Store (SQLite)     │
│  (reqwest + oauth2),         │  │  accounts / account_services│
│  one per enabled service     │  │  / calendars / events /   │
│  (Calendar API today)        │  │  sync_state / pending_edits│
└──────────────────────────────┘  └────────────────────────────┘
```

The UI never talks to the network directly — it reads from and writes to the local SQLite store, and each service's Sync Engine reconciles that store with Google in the background. This is what makes the app work offline: the UI's data source is always local.

## 6. Account & Service Model

Modeled after macOS's Internet Accounts pane: an **account** is just a signed-in identity with a provider (Google, for now); what that account actually *does* in the app is a separate, pluggable set of **services** — Calendar today, with Contacts, Notes, and Tasks as natural future additions on the same account. Adding a new service later means writing a new service module against a shared interface, not redesigning the account system.

**Account vs. service:**

- **Account** = provider + identity + credentials (OAuth tokens in the keyring, profile info: email, display name, avatar). Lives in `accounts` (§8), and knows nothing about calendars, contacts, or any other feature.
- **Service** = a capability an account can provide. Each service type (`Calendar`, later `Contacts` / `Notes` / `Tasks`) is a Rust module implementing a common `Service` interface that owns: its required OAuth scope(s), its own local schema (e.g. Calendar owns `calendars`/`events`; a future Contacts service would own its own `contacts` table), its own sync logic, and the UI view it registers with the shell.
- Whether a given service is turned on for a given account is tracked per pair in `account_services` (§8) — e.g. account "jane@gmail.com" could have Calendar enabled and Contacts disabled.
- The **Sync Engine** in §9 is really **one sync engine instance per enabled (account, service) pair** — each gets its own poll loop, sync token, and failure state, so a future Contacts service on an account wouldn't share cadence or error handling with that same account's Calendar service.

**Account setup UX** mirrors macOS's Internet Accounts: an **Accounts** settings screen lists connected accounts on the left; selecting one shows its available services as toggles — `Calendar` (available, on by default when the account is added), with `Contacts` / `Notes` / `Tasks` shown as **greyed-out "coming soon" toggles** in v1 rather than omitted entirely, so the extensibility is visible in the UI before those services exist. Turning a service on is what triggers that service's OAuth scope request (§7) — a disabled service's scopes are never requested.

**Multi-account behavior** (still true regardless of the above): every connected account can have several calendars (primary, secondary, shared-with-me), each with an assigned display color, user-overridable via the sidebar color picker (§10). The main view is a **unified agenda/calendar** overlaying all enabled calendars from all accounts with Calendar enabled, similar to how the Google Calendar PWA lets you toggle calendars on/off in a sidebar. An account switcher lists each connected account with an avatar/initial and a "Remove account" action. Adding a second, third, etc. account repeats the same OAuth flow (§7); nothing about the flow is account-count-aware.

**Why build the split now, not later:** even though v1 only ships the Calendar service, keeping the Account Manager scoped to identity-only (never assuming "account" implies "calendar account") avoids a rewrite when Contacts/Notes/Tasks get added — those become new `Service` implementations sharing the same account/token/keyring plumbing, added incrementally without touching the Account Manager itself.

## 7. OAuth & Authentication

**Flow: Authorization Code + PKCE, via the system browser and a loopback redirect.** This is Google's current recommendation for installed/desktop apps (the loopback flow remains supported for the "Desktop app" OAuth client type, even though it's been deprecated for Android/iOS/Chrome-app client types).

Step by step, when the user clicks **"Add Google Account"**:

1. App generates a PKCE `code_verifier` and derives `code_challenge`.
2. App starts a short-lived HTTP listener on `127.0.0.1` on an OS-assigned ephemeral port.
3. App opens the user's default browser to Google's OAuth consent screen, requesting scopes only for the service(s) being enabled at add-account time (Calendar's scopes in v1), with `redirect_uri=http://127.0.0.1:<port>` and the PKCE challenge attached.
4. User signs in / picks an account / grants consent in their normal browser (already-logged-in accounts make this a couple of clicks).
5. Google redirects the browser to `http://127.0.0.1:<port>/...?code=...`; the app's loopback listener captures the authorization code and immediately shows a "you can close this tab" page, then shuts the listener down.
6. App exchanges the code + `code_verifier` for an access token and refresh token directly with Google (no client secret needed/trusted, since PKCE is what secures this step for a public client).
7. Refresh token and access token are written to the OS keyring via the Secret Service API — **never to disk in plaintext, never into the SQLite DB.**
8. App creates the `accounts` row (identity only), then creates one `account_services` row per enabled service (Calendar in v1) and lets that service do its own first-run fetch (for Calendar: the account's calendar list).

**Scopes requested:** narrowed to whichever services are enabled — `https://www.googleapis.com/auth/calendar.events` (read/write) and/or `https://www.googleapis.com/auth/calendar.readonly`, only if Calendar is enabled. **Enabling a service later reuses Google's incremental authorization** — the app requests just that service's additional scope(s) against the already-connected account rather than re-running the whole flow from scratch, so turning on a future Contacts/Notes/Tasks service doesn't force the user to re-auth Calendar too.

**Token refresh:** handled transparently by each service's Sync Engine — access tokens are short-lived; the refresh token (long-lived) is used to mint new ones as needed, all via the OS keyring. Tokens are stored per-account, not per-service, since a single OAuth grant on one account can cover multiple enabled services' scopes.

**Removing an account** revokes the token with Google (`POST /revoke`), deletes the keyring entry, and deletes the account's rows (and every enabled service's data) from local storage. **Disabling a single service** on an account (leaving the account itself connected) deletes just that service's local data and drops its scope on next token refresh, without touching the account or its other enabled services.

## 8. Data Model & Local Storage (SQLite)

Rough schema — field lists are illustrative, not exhaustive:

- **`accounts`** — `id`, `google_account_email`, `display_name`, `avatar_url`, `created_at`. Identity only — no service-specific fields. (Tokens live in the OS keyring, keyed by `account.id` — not in this table.)
- **`account_services`** — `account_id` (FK), `service_type` (`calendar`, and future `contacts`/`notes`/`tasks`), `enabled`, `granted_scopes`, `enabled_at`. Composite PK on `(account_id, service_type)`. This is the join table that makes services independently toggleable per account.
- **`calendars`** — `id`, `account_id` (FK, **nullable**), `google_calendar_id` (nullable), `source_url` (nullable — set only for ICS-URL calendars, §11), `display_name`, `color`, `is_visible` (mirrors Google's `calendarList.selected` field, §10), `access_role` (owner/writer/reader — controls whether edits are allowed; always `reader` for ICS-URL calendars). Owned by the Calendar service; a future Contacts service would add its own `contacts` table following the same per-service-ownership pattern rather than growing this table. `account_id`/`google_calendar_id` are only null for the ICS-URL case (§11) — every Google-backed calendar still has both.
- **`events`** — `id`, `calendar_id` (FK), `google_event_id`, `title`, `description`, `location`, `start`, `end`, `all_day`, `recurrence_rule`, `status`, `self_response_status` (accepted/declined/tentative/needsAction — the signed-in account's own RSVP, read off the event's `attendees[]` array; drives "Show declined events" in §12), `etag`, `updated_at`.
- **`sync_state`** — `account_id` (FK), `service_type`, `resource_id` (e.g. a specific `calendar_id` for the Calendar service), `sync_token`, `last_synced_at`, `last_full_sync_at`. Keyed so each (account, service, resource) tuple has independent sync state.
- **`pending_edits`** — `id`, `account_id` (FK), `service_type`, `event_id` (nullable — null for a not-yet-created event), `calendar_id`, `operation` (create/update/delete), `payload` (JSON), `created_at`, `attempt_count`. This is the offline write queue described in §9.
- **`app_settings`** — `key`, `value` (JSON), `updated_at`. A single key/value table backing the Preferences window (§12) — language/region overrides, time zone display settings, event/view/notification defaults, etc. — so preferences sync with the rest of the app's state instead of living in a separate config file.

## 9. Sync Engine

This section describes the Calendar service's sync engine specifically — per §6, each enabled (account, service) pair gets its own independent instance of the pattern below, so a future Contacts/Notes/Tasks service would follow the same shape without sharing state with Calendar's.

**Why polling, not push:** Google Calendar API's `events.watch` push mechanism only delivers change notifications to a public HTTPS webhook — unlike the Gmail API, which also supports a *pull*-based Cloud Pub/Sub subscription, Calendar API has no such pull option. A purely local, backend-free desktop app has no public endpoint to receive a webhook at, so true push is not achievable without hosting a relay service (deferred — see §20). Instead, Calendarchy uses **cheap, delta-only polling**.

- **Initial sync** (new account or new calendar becoming visible): a full `events.list` fetch, storing the resulting `nextSyncToken`.
- **Incremental sync**: subsequent `events.list` calls pass `syncToken`, so Google returns only what changed since last time — cheap enough to poll frequently without hitting quota.
- **Poll interval**: default every 5 minutes per account, user-configurable. Kept per-account so one account's polling cadence doesn't depend on another's.
- **Trigger-based syncs**, independent of the timer: app gains focus, network reconnects, system wakes from suspend, right after the user makes a local edit (to push it promptly), and a manual "Refresh" action.
- **`410 GONE` handling**: if Google reports the stored `syncToken` has expired, drop it and fall back to a full resync for that calendar.
- **Offline edits**: writes made while offline (or while a sync is failing) go into `pending_edits` and are applied optimistically to the local `events` table so the UI reflects them immediately. The Sync Engine drains this queue opportunistically whenever a sync succeeds, in creation order.
- **Conflict handling**: Calendar API's `etag`/`updatedAt` semantics are used to detect if the server-side event changed since the local edit was queued. On conflict, the simplest correct behavior is **server wins, local edit surfaced to the user as a re-apply prompt** rather than silently dropped or silently overwriting — full auto-merge is out of scope for v1.

## 10. Calendar UI/UX

- **Left sidebar**: collapsible via a hamburger icon in the headerbar (hides/shows the whole pane, independent of any individual calendar's visibility). Top to bottom: a **Create** button opening the event editor below — its dropdown arrow mirrors Google's quick-create menu, but only the "Event" entry does anything in v1 (Task is greyed out pending the future Tasks service, §6; Appointment schedule and Out of office are Workspace-booking features and out of scope, see below); a **mini-month date navigator** (click a date to jump the main view there, arrows to page months, independent of whichever main view is active); the **World clock** module (§12) directly underneath, when enabled; a **Search for people** box — an ad hoc, non-persistent overlay that looks up a colleague by email and temporarily shows their availability in the main grid without adding anything to a calendar list (contrast with §11's Subscribe to calendar, which does persist); and two collapsible checklists, **My calendars** (this account's own primary/secondary calendars) and **Other calendars** (anything added via §11's Subscribe/Browse/From-URL flows), each row wired to the per-calendar sidebar menu below. **Booking pages** — Google Workspace's appointment-scheduling links — is out of scope; it's a scheduling product layered on top of Calendar, not a calendar-viewing/editing capability (§2).
- **Views**: Month, Week, Day, Year, and Agenda (list) — matching the core Google Calendar PWA view set — plus a **5-day work week** view (Mon–Fri, Google's "5 days" option). Year is the cheapest of the six to implement: a grid of 12 mini-months for navigation, with no per-event rendering needed.
- **Event editor**: a libadwaita dialog for title, time (with all-day toggle), location, description, calendar/account picker, and reminder lead time.
- **Multi-calendar overlay**: all visible calendars render together, color-coded, with a sidebar checklist to toggle visibility per calendar (mirrors the Google Calendar PWA sidebar).
- **Per-calendar sidebar menu**: each sidebar row's "⋮" menu adds **Display this only** (isolate — flip `selected=true` on this calendar and `false` on every other enabled one) and **Hide from list** (the same visibility flag the checklist checkbox already flips, §8's `is_visible`), both applied via `calendarList.patch`, plus a **color picker**: the fixed palette from `colors().get()` (calendar colors, not event colors) applied via `calendarList.patch({colorId})`, with a "+" custom swatch that sets `backgroundColor`/`foregroundColor` directly instead. **Settings and sharing** opens a per-calendar dialog covering only the in-scope half of what Google's own dialog shows — name, description, notification defaults (§12) — the sharing/ACL half is out of scope per §2 (Calendarchy consumes calendars, it doesn't administer them).
- **Search**: simple local full-text filter over cached event titles/descriptions/locations (searches the local store, so it works offline too, at the cost of only covering already-synced events).
- **Keyboard navigation**: arrow keys / vim-style `hjkl` to move between days/weeks, `n` for new event, `/` to search, and Google's own single-letter view shortcuts reused as-is — `D`/`W`/`M`/`Y`/`A`/`X` for Day/Week/Month/Year/Agenda/5-day work week — rather than inventing Calendarchy-specific bindings.

## 11. Adding Calendars

Google Calendar's own "+ Add calendar" affordance covers four distinct ways of growing what shows up in the sidebar beyond the calendars an account already has by default. Calendarchy mirrors the same four entry points, each mapping to a different Calendar API call — or, in one case, to no API at all:

- **Subscribe to calendar** — look up another person's or resource's calendar by email and add it to your list, provided they've shared it with you (or it's a public/domain calendar). Maps directly to `calendarList.insert({calendarId})` against the already-connected account — no new OAuth scope, no new account, just a new row appended to that account's calendar list and a new local `calendars` row (§8) once Google confirms access.
- **Create new calendar** — creates a brand-new secondary calendar (name, description, time zone) owned by the connected Google account, via `calendars.insert`. It then appears in that account's `calendarList` and gets pulled into local storage on the next sync (§9) like any other calendar.
- **Browse calendars of interest** — Google's curated picker (regional holidays, phases of the moon, sports schedules) has no public API for *listing* the curated set — that catalog lives inside the Calendar web client, not Calendar API v3. Calendarchy ships its own small hardcoded shortlist of well-known public calendar IDs (e.g. `en.usa#holiday@group.v.calendar.google.com` and other locale-appropriate holiday calendars, keyed off the region setting in §12) and subscribes to a picked one the same way as "Subscribe to calendar" — `calendarList.insert` with that known ID. This shortlist is necessarily narrower than Google's own until a public catalog exists.
- **From URL** — subscribing to an arbitrary external `.ics` feed. This is the one flow with no public Calendar API equivalent: Google's own "From URL" feature is handled by the web client against an undocumented internal endpoint, not `calendars.insert` or `calendarList.insert`. Rather than reverse-engineer a private API, v1 treats externally-hosted ICS feeds as their own lightweight, account-less local calendar type: Calendarchy fetches and parses the feed directly (read-only, no OAuth, no Google account involved, `account_id`/`google_calendar_id` left null per §8) and polls it on the same cadence as §9's sync loop. This sits just outside the "Google Calendar only" boundary in §2, but it's additive rather than a new provider integration — no write support, no accounts, no sync-token/conflict machinery, just a periodically-refreshed read-only event source layered into the same local `calendars`/`events` tables.

## 12. Settings & Preferences

A single libadwaita **Preferences** window (`Ctrl+,`, or from the app menu) holds every user-configurable knob that doesn't belong to a specific object (an account, a calendar, an event). It's organized around the same categories Google Calendar's own gear-icon Settings menu exposes, so anyone coming from the PWA finds the same knobs in roughly the same places. Values are stored in the `app_settings` key/value table (§8) rather than a separate config file, so preferences travel with the rest of the app's state.

- **Language and region** — no separate in-app picker in v1. Calendarchy reads the system locale (`$LANG`/`LC_TIME`), the same way it inherits Omarchy's GTK4 theme (§14) rather than shipping its own — UI language, date order, first-day-of-week, and 12h/24h clock all follow whatever the desktop is already set to. An in-app override is future work, only if someone wants the app's language independent of their desktop locale.
- **Time zone** — defaults to the system time zone (`/etc/localtime`) for both the calendar grid and new-event creation. A secondary "display time zone" can be set here, which adds a second time gutter to Week/Day views (§10) — useful for scheduling across zones without changing every event's zone. Per-event zone override still lives in the event editor (§10), independent of this default.
- **World clock** — an opt-in sidebar module (off by default) listing a user-picked set of cities/zones as small clocks directly under the sidebar's mini-month date navigator (§10), mirroring Google Calendar's World Clock panel. A nice-to-have that doesn't gate any phase — picked up opportunistically alongside the other polish-phase items in §19.
- **Event settings** — defaults applied by the event editor (§10) when creating a new event: default duration (e.g. 30/60 min), default calendar (which account+calendar a quick-add lands in), and default reminder lead time (feeds §13's Notification Scheduler unless overridden per-event). No "speedy meetings" auto-shortening or other Workspace-side event defaults — those are computed server-side by Google's own client (see Google Workspace smart features, below).
- **Notification settings** — global defaults layered under the per-event lead time in the editor: desktop-notification on/off per calendar (mute a noisy calendar without disabling its sync), a notification sound toggle, and the default lead time used when an event doesn't specify its own. The delivery mechanism itself is §13.
- **View options** — default view on launch (any view from §10, including Year and the 5-day work week), density (comfortable/compact row height), **Show weekends**, and **Show declined events** (dimmed rather than hidden, driven by the new `self_response_status` field on `events`, §8). **Show completed tasks** appears in the same list but greyed out — "coming soon," the same treatment §6 gives the Contacts/Notes/Tasks service toggles — until the Tasks service actually exists to supply completed-task data. Start-of-week here is a per-app override of the region default above, matching Google Calendar's own split between region-implied and explicitly-set start day.
- **Google Workspace smart features** — explicitly **not implemented** (§2). Google's smart features are computed by Google's own backend against broader Workspace scopes (Gmail, Drive) this app deliberately doesn't request, kept as narrow as each service's actual usage needs (§15), and aren't exposed through the Calendar API a pure-calendar client consumes. Tracked as a non-goal rather than a setting with no effect.
- **Keyboard shortcuts** — a read-only `Gtk::ShortcutsWindow` cheat sheet (opened with `?` or from Preferences) listing the bindings already defined in §10. In-app rebinding is out of scope for v1; the shortcuts are fixed, not configurable.
- **Offline** — no toggle, unlike the Calendar PWA where offline mode is opt-in. Calendarchy is offline-first by construction (§5, §9): the UI always reads from the local SQLite cache regardless of connectivity. What *does* live in this settings pane is the operational surface around that: each account's poll interval override (§9 defaults to 5 minutes), a manual "Sync now" action, and a "Clear local cache and resync" action for recovering from a corrupted cache without deleting the account.

## 13. Notification Panel Integration

Two layers, deliberately separated by confidence level:

**a) Standard desktop notifications (solid, v1)** — Event reminders are delivered via the freedesktop `org.freedesktop.Notifications` D-Bus interface. Quickshell's unified shell still implements this interface as the system's notification server (Quattro consolidated *where* notifications are handled, not the standard they're delivered over), so this integration is not Quattro-specific plumbing — it's the same standard interface any Linux desktop notification goes through, and it lands in Omarchy's native notification center like any other system notification. A background **Notification Scheduler** component watches upcoming events (from the local store — no network needed) and fires a D-Bus notification at each event's configured lead time, with actions like "Snooze" / "Dismiss" wired to the notification's action buttons.

**b) Quickshell panel widget (stretch, not committed for v1)** — An "upcoming events" module living directly in Quickshell's panel/notification-center UI, driven over Quickshell's IPC. This is called out as a stretch goal because it depends on Quickshell's plugin/IPC API surface, which is newer and less documented than the freedesktop notification standard; §20 tracks this as something to scope once (a) is working and the Quickshell plugin API is better understood.

## 14. System Integration

- **`.desktop` launcher entry** with a proper icon, so the app shows up in Omarchy's launcher like any other app.
- **Autostart** (optional, user-toggleable) so reminders keep firing even if the main window isn't open — the app can run with the window hidden/minimized to tray-equivalent state.
- **Single-instance enforcement** via D-Bus well-known name ownership — launching the app again focuses the existing window instead of opening a second instance.
- **Hyprland window rules** — sensible defaults suggested in the doc/README (e.g., floating vs tiled preference), left to the user's own Hyprland config rather than the app forcing behavior.
- **Theming** — no custom CSS theme; GTK4/libadwaita picks up Omarchy's system theme (accent color, light/dark) automatically.

## 15. Security & Privacy

- PKCE means no client secret has to be trusted/embedded in a public client binary.
- OAuth tokens live only in the OS keyring (Secret Service), never in the SQLite DB or in plaintext config files.
- Scopes are requested per-service, only for services the user has enabled on that account, and kept as narrow as each service's actual usage needs (§7).
- All calendar data lives locally in the user's own SQLite file; nothing is sent anywhere except directly to Google's API.

## 16. Packaging & Distribution

- Standard `cargo build --release` producing a single native binary.
- Distributed as an **AUR package** (`PKGBUILD`) — the natural distribution channel for an Arch/Omarchy-native app, and installable the same way users already install everything else on Omarchy.
- Semantic versioning starting at `0.1.0` through the MVP phases in §19.

## 17. Error Handling & Resilience

- Network loss: sync attempts fail silently in the background (surfaced subtly in the UI, e.g. a small "offline" indicator), queued edits remain queued, app stays fully usable against the local cache.
- Revoked/expired refresh token: account is marked "needs re-authentication" in the account switcher; a click re-runs the OAuth flow (§7) rather than silently dropping the account.
- API rate limiting: exponential backoff on 429/5xx responses from the Calendar API, scoped per-account so one rate-limited account doesn't stall others.
- Partial failures are isolated per-account — one account's sync error is surfaced (e.g., a small warning badge) without blocking sync or UI for the other accounts.

## 18. Testing Strategy

- Unit tests for the Sync Engine's delta-application and conflict-resolution logic, against a mocked Calendar API client (trait-based, so a fake implementation can be swapped in for tests).
- Integration tests for the local SQLite layer (schema migrations, CRUD, query correctness for calendar views).
- OAuth loopback flow is inherently hard to fully automate (it opens a real browser); document a manual test checklist for it rather than trying to fully automate.

## 19. Phased Roadmap

0. **Account/Service foundation** — Account Manager (identity + OAuth + keyring) and the `Service` trait/registry from §6 built first, with Calendar as the only registered service. Everything below is the Calendar service built on top of this foundation, so later services slot in without revisiting it.
1. **MVP** — single account, Calendar service only, read-only Month + Agenda views, local SQLite cache, basic full sync.
2. **Multi-account** — account switcher, unified overlay view, per-calendar color/visibility, Accounts settings screen showing the (mostly greyed-out) service toggles from §6.
3. **Write support** — event create/edit/delete, offline edit queue, conflict handling.
4. **Reminders & notifications** — Notification Scheduler, D-Bus desktop notifications with actions.
5. **Polish & packaging** — Week/Day/Year/5-day-work-week views, view options (§12), search, keyboard nav completeness, AUR packaging, Hyprland/theming polish.

Quickshell panel widget integration (§13b) is intentionally not a numbered phase — it's picked up opportunistically once phase 4 is stable. Contacts/Notes/Tasks services (§6) are intentionally not numbered phases either — they're future work enabled by phase 0, not committed to a timeline here.

## 20. Open Questions / Future Enhancements

- **True push via relay backend**: if polling latency (up to the poll interval) proves unacceptable, a future version could add a small always-on relay service that owns the HTTPS webhook Google requires, registers `events.watch` channels on the user's behalf, and forwards change pings to the running app over a persistent connection. Deliberately out of scope for v1 — it turns a purely local native app into one with a hosted dependency.
- **Recurring event edit scope**: Google's recurring-event model (single event vs. recurring series vs. "this and following") is one of the fiddlest parts of the Calendar API to get right in an editor UI; v1 should scope this down explicitly rather than attempt full parity with the Google Calendar PWA's recurrence editor on day one.
- **New services on existing accounts** (Contacts, Notes, Tasks): the extensibility axis §6 is built for — new `Service` implementations reusing the same Account Manager, each with its own Google API (People API for Contacts, Tasks API for Tasks, etc.), own schema, own sync engine instance. No architecture changes anticipated to add one, only new code.
- **New providers** (CalDAV, iCloud, Outlook/Microsoft Graph): a separate, orthogonal extensibility axis from services — today `accounts` implicitly assumes a Google identity; broadening `accounts` to be provider-tagged (so a `Service` implementation can be asked to work against more than one provider's API) is future work, not started in v1.
- **Quickshell panel widget** (§13b): scope this once the freedesktop-notification path (§13a) is shipped and the Quickshell plugin/IPC API has been evaluated hands-on.
