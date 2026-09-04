use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::storage::Storage;

/// The whole Preferences window (DESIGN_SPEC.md §12) is backed by one JSON blob under
/// this single `app_settings` key, rather than one row per field — simpler to load and
/// save atomically, and `app_settings`' schema (§8) only requires a JSON `value` per
/// `key`, not one row per setting.
const SETTINGS_KEY: &str = "preferences";

/// Who Google Calendar's own event-creation flow lets an invited guest add to their
/// calendar, mirrored here as a stored default rather than an enforced permission —
/// same "stored ahead of the feature" reasoning as `default_event_duration_minutes`
/// below, since event creation doesn't send guest/invitation fields to the Calendar
/// API yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvitationAutoAdd {
    All,
    KnownSenders,
    No,
}

impl Default for InvitationAutoAdd {
    fn default() -> Self {
        InvitationAutoAdd::KnownSenders
    }
}

/// Overrides the day/month/year order the app displays dates in (DESIGN_SPEC.md §12's
/// Language and region section). `System` — the default — doesn't pick a display order
/// itself; the app crate resolves it to a concrete order from the desktop locale (see
/// `resolve_date_format` in `crates/app/src/main.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DateFormat {
    System,
    MonthDayYear,
    DayMonthYear,
    YearMonthDay,
}

impl Default for DateFormat {
    fn default() -> Self {
        DateFormat::System
    }
}

/// Overrides 12-hour vs. 24-hour clock display (DESIGN_SPEC.md §12's Language and
/// region section). `System` — the default — is resolved to a concrete choice from the
/// desktop locale by `resolve_time_format` in the app crate, same as `DateFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeFormat {
    System,
    TwelveHour,
    TwentyFourHour,
}

impl Default for TimeFormat {
    fn default() -> Self {
        TimeFormat::System
    }
}

/// Every user-configurable knob the Preferences window exposes. `#[serde(default)]`
/// on each field (via the container attribute) means a settings blob written by an
/// older version — missing a field this version added — still deserializes instead of
/// falling back to `AppSettings::default()` wholesale and losing every other saved
/// preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Explicit override for the "primary" time zone (IANA name, e.g.
    /// `"America/New_York"`) used by the calendar grid and new-event creation.
    /// `None` means "follow the system time zone" (`/etc/localtime`), §12's default.
    pub primary_timezone: Option<String>,
    /// User-chosen short label shown next to the primary time zone (e.g. `"EST"`),
    /// purely cosmetic — mirrors the free-text "Label" field in Google Calendar's own
    /// Settings ▸ Time zone panel, which this section's layout follows.
    pub primary_timezone_label: Option<String>,
    /// Whether Week/Day views should render a second time gutter alongside the
    /// primary one. Kept independent of `secondary_timezone` (rather than inferring
    /// "on" from `Some`) so unchecking this preserves the previously chosen secondary
    /// zone/label instead of discarding them — same as Google Calendar's own checkbox.
    pub display_secondary_timezone: bool,
    /// IANA zone name for the optional second time gutter in Week/Day views (§12) —
    /// meaningful only while `display_secondary_timezone` is true. Week/Day views
    /// aren't built yet (§19 phase 5), so this is stored but has no visible effect
    /// until they land.
    pub secondary_timezone: Option<String>,
    /// User-chosen short label for the secondary time zone (e.g. `"CST"`), same as
    /// `primary_timezone_label`.
    pub secondary_timezone_label: Option<String>,
    /// Mirrors Google Calendar's "Ask to update my primary time zone to current
    /// location" checkbox — whether the app should prompt to reset `primary_timezone`
    /// back to `None` (follow system) when it detects the user's location no longer
    /// matches it. No geolocation lookup exists in this app yet, so this is stored but
    /// has no effect; defaults to `true` to match Google Calendar's own default.
    pub ask_update_primary_timezone_to_location: bool,
    /// Default duration, in minutes, applied when creating a new event (§12). Event
    /// creation isn't built yet (§19 phase 3) — stored ahead of that feature the same
    /// way `account_services` already models Contacts/Notes/Tasks before those
    /// services exist (§6).
    pub default_event_duration_minutes: i64,
    /// Default reminder lead time, in minutes, for a newly created event (§12).
    pub default_reminder_minutes: i64,
    /// Which calendar a new event lands on by default; `None` means "ask each time".
    /// References `calendars.id` loosely (not a DB foreign key, since this lives in
    /// `app_settings` rather than a calendar-owned table) — a deleted calendar simply
    /// falls back to "ask each time" the next time this is read.
    pub default_calendar_id: Option<i64>,
    /// Tighter grid/sidebar spacing (§12's view density option) — the one setting here
    /// with an immediate visual effect, applied via a CSS class on the main window.
    pub compact_density: bool,
    /// Master on/off for event-reminder desktop notifications (§12/§13). Distinct from
    /// `default_reminder_minutes` (how far ahead a reminder fires) — this is whether
    /// the Notification Scheduler delivers them at all once it exists.
    pub desktop_notifications_enabled: bool,
    /// Whether a reminder notification plays a sound on delivery, alongside the
    /// desktop popup (§12's "notification sound toggle").
    pub play_notification_sounds: bool,
    /// How long a reminder's Snooze button delays it by, in minutes from when it's
    /// clicked — the floating notification dialog's Snooze button applies this by
    /// default, with a dropdown next to it for picking a different duration for that
    /// one alert (§13) without changing this default.
    pub default_snooze_minutes: i64,
    /// When true, only events the user has RSVP'd "Yes" or "Maybe" to (per
    /// `events.self_response_status`, §8) trigger a reminder notification — declined
    /// or not-yet-responded events stay silent.
    pub notify_only_if_responded: bool,
    /// Default `guestsCanModify` sent on events this account creates — mirrors Google
    /// Calendar's "Guest permissions ▸ Modify event" checkbox, unchecked by default.
    /// Stored ahead of the feature (§12): event creation doesn't send guest-permission
    /// fields to the Calendar API yet.
    pub guests_can_modify: bool,
    /// Default `guestsCanInviteOthers`, mirroring "Guest permissions ▸ Invite others".
    pub guests_can_invite_others: bool,
    /// Default `guestsCanSeeOtherGuests`, mirroring "Guest permissions ▸ See guest list".
    pub guests_can_see_guest_list: bool,
    /// Mirrors Google Calendar's "Add invitations to my calendar" dropdown — which
    /// received invitations get auto-added rather than left pending in the inbox view.
    pub invitation_auto_add: InvitationAutoAdd,
    /// Mirrors "Let others see all invitations if they have permission to view or edit
    /// my events" — whether a delegate with edit access to this calendar also sees
    /// invitations still awaiting this user's RSVP.
    pub show_all_invitations_to_editors: bool,
    /// Mirrors "Automatically add Google Meet video conferences to events I create" —
    /// whether new events get `conferenceData` populated by default. Checked by default,
    /// matching Google Calendar's own default.
    pub auto_add_google_meet: bool,
    /// "Show weekends" (§12's View options group) — whether Saturday/Sunday columns
    /// render. The 5-day view consults this (`five_day_window` in `crates/app`:
    /// on, a rolling calendar-day window that may include a weekend; off, a rolling
    /// business-day-only window). The month grid is Sunday-first and always renders
    /// all seven columns today, so Month/Week don't consult this yet.
    pub show_weekends: bool,
    /// "Show declined events" (§12) — dims rather than hides events the signed-in
    /// account has declined, per `events.self_response_status` (§8). No view reads
    /// `self_response_status` yet, so declined events render like any other.
    pub show_declined_events: bool,
    /// "Show completed tasks" (§12) — greyed out in the Preferences UI, like the
    /// Contacts/Notes/Tasks service toggles (§6), until the Tasks service exists to
    /// supply completed-task data.
    pub show_completed_tasks: bool,
    /// "Show week numbers" (§12) — an ISO week-number gutter alongside Month/Week
    /// grids. Not rendered yet.
    pub show_week_numbers: bool,
    /// "Display shorter events the same size as 30 minute events" (§12) — floors an
    /// event chip's rendered height at the 30-minute size instead of shrinking it to
    /// fit the event's actual duration. Not applied yet — the grid doesn't render
    /// events at all today (§19 phase 1 covers read-only Month + Agenda).
    pub uniform_short_event_height: bool,
    /// "Reduce the brightness of past events" (§12) — dims events whose end time has
    /// already passed. Not applied yet, same reason as `uniform_short_event_height`.
    pub dim_past_events: bool,
    /// "View calendars side by side in Day View" (§12) — Day view renders one column
    /// per visible calendar instead of overlaying them. Day view itself isn't built
    /// yet (§19 phase 5), so this has no visible effect until it lands.
    pub side_by_side_calendars_in_day_view: bool,
    /// "Time scale" (§12) — minutes represented by each row in Day view's hour grid.
    /// One of 5/10/15/30/60; smaller values zoom in (more, finer gridlines; taller
    /// scrollable grid) and larger values zoom out. Adjustable from Preferences or via
    /// Ctrl+scroll directly on the Day view grid, which keeps this setting in sync
    /// (`crates/app/src/main.rs`'s `AppMsg::ZoomDayTimeScale`) so either control
    /// reflects the other's changes.
    pub day_time_scale_minutes: i64,
    /// Snap granularity (minutes) for a Day view drag while Ctrl is held — one of two
    /// configurable overrides of the plain-drag default (which snaps to
    /// `day_time_scale_minutes`'s gridlines instead). Checked live for the whole
    /// gesture (`crates/app/src/main.rs`'s `install_day_event_drag`), not just at
    /// drag-start, so releasing/re-pressing Ctrl mid-drag changes the snap immediately.
    pub day_drag_snap_ctrl_minutes: i64,
    /// Snap granularity (minutes) for a Day view drag while Ctrl+Shift is held — the
    /// finer of the two override tiers (checked before the plain-Ctrl one, since
    /// Ctrl+Shift also matches a plain "is Ctrl held" test). See
    /// `day_drag_snap_ctrl_minutes`.
    pub day_drag_snap_ctrl_shift_minutes: i64,
    /// "Start week on" (§12) — a per-app override of the region-implied first day of
    /// week, stored as days-from-Sunday (0 = Sunday .. 6 = Saturday), matching
    /// `chrono::Weekday::num_days_from_sunday`. `None` means "follow region" (the
    /// system locale default §12 describes); the month grid is hardcoded Sunday-first
    /// today regardless of this setting.
    pub start_of_week: Option<u8>,
    /// Day count for the sidebar's "Set custom view" picker (§10's 5-day work week is
    /// this setting's default) — independent of that fixed Mon–Fri preset. No custom
    /// view exists yet to apply it to.
    pub custom_view_days: i64,
    /// Alternate/lunar calendar overlay shown alongside grid dates (§12) — `None`
    /// ("None" in the picker) or a lowercase system name (`"chinese"`, `"hebrew"`,
    /// `"islamic"`). No overlay is rendered yet.
    pub alternate_calendar: Option<String>,
    /// BCP-47-ish language tag (e.g. `"en-US"`), overriding the system locale (§12's
    /// Language and region section). Calendarchy has no translation catalog yet, so
    /// this doesn't retranslate any UI text — it's stored ahead of that future work,
    /// same reasoning as `default_event_duration_minutes` above. `None` means "follow
    /// the system locale".
    pub language_override: Option<String>,
    /// ISO 3166-1 alpha-2 country code (e.g. `"US"`), overriding the system locale's
    /// implied region (§12). Not yet consumed anywhere — DESIGN_SPEC.md §11's
    /// region-keyed holiday-calendar shortlist is the first planned consumer. `None`
    /// means "follow the system locale".
    pub country_override: Option<String>,
    /// Overrides date display order app-wide (event editor date pickers, event
    /// popover date line) — see `DateFormat`.
    pub date_format: DateFormat,
    /// Overrides 12h/24h clock display app-wide (event chips, event popover time
    /// range) — see `TimeFormat`.
    pub time_format: TimeFormat,
    /// Whether the sidebar's World Clock module (§10/§12) is shown at all — off by
    /// default, mirroring the Google Calendar PWA's opt-in World Clock panel.
    pub world_clock_enabled: bool,
    /// IANA zone names shown in the sidebar's World Clock module, top to bottom — this
    /// order is exactly the render order, and is user-controlled via the Preferences
    /// window's reorder buttons rather than being alphabetized or otherwise derived.
    pub world_clock_zones: Vec<String>,
    /// Whether Calendarchy's own floating multi-alert dialog (this feature) shows
    /// reminders, independent of `desktop_notifications_enabled` (the OS toast) — a
    /// user can run either surface, both, or neither.
    pub custom_notification_dialog_enabled: bool,
    /// Absolute path to a user-picked custom reminder sound file. `None` means "use
    /// the desktop's default freedesktop sound theme event sound" (resolved at
    /// play-time — see `crates/app/src/notifications.rs`), not "no sound".
    pub notification_sound_path: Option<String>,
    /// Last dragged position of the floating notification dialog, stored as
    /// `(margin_top, margin_right)` layer-shell margins from its fixed top-right
    /// anchor. `None` means it hasn't been moved yet and opens at the built-in
    /// default offset.
    pub notification_dialog_position: Option<(i32, i32)>,
    /// Sidebar width, as a fraction of the window's width at the time it was last
    /// dragged (sidebar_px / window_px) rather than a raw pixel count — so the panel
    /// keeps looking proportionally the same after the window moves to a monitor with
    /// different scaling/DPI or a different size. `None` means "not yet customized,
    /// use the built-in default" (the old fixed 240px sidebar against the 1100px
    /// default window width).
    pub sidebar_width_fraction: Option<f64>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            primary_timezone: None,
            primary_timezone_label: None,
            display_secondary_timezone: false,
            secondary_timezone: None,
            secondary_timezone_label: None,
            ask_update_primary_timezone_to_location: true,
            default_event_duration_minutes: 30,
            default_reminder_minutes: 10,
            default_calendar_id: None,
            compact_density: false,
            desktop_notifications_enabled: true,
            play_notification_sounds: true,
            default_snooze_minutes: 10,
            notify_only_if_responded: false,
            guests_can_modify: false,
            guests_can_invite_others: true,
            guests_can_see_guest_list: true,
            invitation_auto_add: InvitationAutoAdd::KnownSenders,
            show_all_invitations_to_editors: false,
            auto_add_google_meet: true,
            show_weekends: true,
            show_declined_events: true,
            show_completed_tasks: true,
            show_week_numbers: false,
            uniform_short_event_height: false,
            dim_past_events: true,
            side_by_side_calendars_in_day_view: true,
            day_time_scale_minutes: 60,
            day_drag_snap_ctrl_minutes: 5,
            day_drag_snap_ctrl_shift_minutes: 1,
            start_of_week: None,
            custom_view_days: 5,
            alternate_calendar: None,
            language_override: None,
            country_override: None,
            date_format: DateFormat::System,
            time_format: TimeFormat::System,
            world_clock_enabled: false,
            world_clock_zones: Vec::new(),
            custom_notification_dialog_enabled: true,
            notification_sound_path: None,
            notification_dialog_position: None,
            sidebar_width_fraction: None,
        }
    }
}

/// Loads the saved preferences, or `AppSettings::default()` if none have been saved
/// yet (first launch) or the stored JSON fails to parse (e.g. hand-edited DB).
pub fn load_settings(storage: &Storage) -> anyhow::Result<AppSettings> {
    storage.with_conn(|conn| {
        let raw: Option<String> = conn
            .query_row("SELECT value FROM app_settings WHERE key = ?1", [SETTINGS_KEY], |row| row.get(0))
            .optional()?;
        Ok(raw
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default())
    })
}

/// Persists the full settings blob, overwriting whatever was saved before.
pub fn save_settings(storage: &Storage, settings: &AppSettings) -> anyhow::Result<()> {
    let json = serde_json::to_string(settings)?;
    storage.with_conn(|conn| {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![SETTINGS_KEY, json],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_settings_defaults_when_nothing_saved_yet() {
        let storage = Storage::open_in_memory().expect("open");
        let settings = load_settings(&storage).expect("load");
        assert_eq!(settings, AppSettings::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let storage = Storage::open_in_memory().expect("open");
        let mut settings = AppSettings::default();
        settings.primary_timezone = Some("Europe/London".into());
        settings.primary_timezone_label = Some("LON".into());
        settings.display_secondary_timezone = true;
        settings.secondary_timezone = Some("America/New_York".into());
        settings.secondary_timezone_label = Some("EST".into());
        settings.ask_update_primary_timezone_to_location = false;
        settings.compact_density = true;
        settings.default_calendar_id = Some(42);
        settings.desktop_notifications_enabled = false;
        settings.play_notification_sounds = false;
        settings.default_snooze_minutes = 5;
        settings.notify_only_if_responded = true;
        settings.guests_can_modify = true;
        settings.guests_can_invite_others = false;
        settings.guests_can_see_guest_list = false;
        settings.invitation_auto_add = InvitationAutoAdd::All;
        settings.show_all_invitations_to_editors = true;
        settings.auto_add_google_meet = false;
        settings.show_weekends = false;
        settings.show_declined_events = false;
        settings.show_completed_tasks = false;
        settings.show_week_numbers = true;
        settings.uniform_short_event_height = true;
        settings.dim_past_events = false;
        settings.side_by_side_calendars_in_day_view = false;
        settings.day_time_scale_minutes = 15;
        settings.day_drag_snap_ctrl_minutes = 7;
        settings.day_drag_snap_ctrl_shift_minutes = 2;
        settings.start_of_week = Some(1);
        settings.custom_view_days = 4;
        settings.alternate_calendar = Some("chinese".into());
        settings.language_override = Some("fr".into());
        settings.country_override = Some("FR".into());
        settings.date_format = DateFormat::DayMonthYear;
        settings.time_format = TimeFormat::TwentyFourHour;
        settings.world_clock_enabled = true;
        settings.world_clock_zones = vec!["Europe/London".into(), "Asia/Tokyo".into()];
        settings.custom_notification_dialog_enabled = false;
        settings.notification_sound_path = Some("/home/user/sounds/chime.ogg".into());
        settings.notification_dialog_position = Some((64, 32));
        settings.sidebar_width_fraction = Some(0.3);
        save_settings(&storage, &settings).expect("save");

        let loaded = load_settings(&storage).expect("load");
        assert_eq!(loaded, settings);
    }

    #[test]
    fn saving_twice_overwrites_rather_than_duplicating() {
        let storage = Storage::open_in_memory().expect("open");
        save_settings(&storage, &AppSettings::default()).expect("save 1");
        let mut second = AppSettings::default();
        second.default_event_duration_minutes = 60;
        save_settings(&storage, &second).expect("save 2");

        let row_count: i64 = storage
            .with_conn(|conn| Ok(conn.query_row("SELECT count(*) FROM app_settings", [], |r| r.get(0))?))
            .expect("count");
        assert_eq!(row_count, 1);
        assert_eq!(load_settings(&storage).expect("load").default_event_duration_minutes, 60);
    }
}
