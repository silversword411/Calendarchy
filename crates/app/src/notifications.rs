//! The Notification Scheduler and its two delivery surfaces (DESIGN_SPEC.md §13): the
//! OS desktop toast (via `calendarchy_core::notifier`, Omarchy's own
//! `org.freedesktop.Notifications` service) and Calendarchy's own floating
//! multi-alert dialog. Split out of `main.rs` because this feature is substantial
//! enough (a poll loop, a layer-shell window, drag-to-reposition, sound playback,
//! snooze/dismiss/history) to warrant its own file rather than growing `main.rs`
//! further.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use calendarchy_core::{load_settings, save_settings, DbusNotifier, NotifyRequest, Storage, SystemNotifier, TimeFormat, Urgency};
use calendarchy_service_calendar::query::{
    active_reminder_notifications, due_reminders, record_fired, recent_reminder_history, ActiveReminder, DueReminder,
    ReminderHistoryEntry,
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::{ComponentSender, RelmWidgetExt};

use crate::{App, AppMsg};

const POLL_INTERVAL: Duration = Duration::from_secs(20);
const HISTORY_LIMIT: i64 = 20;
const DEFAULT_MARGIN_TOP: i32 = 48;
const DEFAULT_MARGIN_RIGHT: i32 = 24;
/// Shipped by the `sound-theme-freedesktop` package, already installed on Omarchy —
/// used as the default chime so no binary audio asset needs to live in this repo.
const DEFAULT_SOUND_PATH: &str = "/usr/share/sounds/freedesktop/stereo/message-new-instant.oga";

/// Starts the Notification Scheduler's poll loop (DESIGN_SPEC.md §13) — a
/// `glib::timeout_add_local` tick, the same trigger idiom `start_world_clock_ticker`
/// already uses, whose body just hands a plain `async fn` to relm4's own
/// process-lifetime tokio runtime via `oneshot_command` (the same mechanism
/// `sync_all_accounts`/`clear_cache_and_resync_all` already use) — no new thread or
/// runtime plumbing needed.
pub fn start_scheduler(sender: ComponentSender<App>, storage: Storage) {
    gtk4::glib::timeout_add_local(POLL_INTERVAL, move || {
        sender.oneshot_command(check_and_fire_reminders(storage.clone()));
        gtk4::glib::ControlFlow::Continue
    });
}

/// One poll tick: loads settings, and — unless both delivery surfaces are off, in
/// which case this is a cheap early-out with no DB hit — finds every due reminder,
/// records it as fired, sends the OS toast if `desktop_notifications_enabled`, and
/// plays the reminder sound once per batch (not once per alert) if
/// `play_notification_sounds`. Returns the newly-fired batch so the GTK thread can
/// push it into the floating dialog — everything here runs off the GTK thread.
pub async fn check_and_fire_reminders(storage: Storage) -> crate::AppCommandMsg {
    let settings = load_settings(&storage).unwrap_or_default();
    if !settings.desktop_notifications_enabled && !settings.custom_notification_dialog_enabled {
        return crate::AppCommandMsg::ReminderCheckFinished { newly_fired: Vec::new() };
    }

    let now = Utc::now();
    let due = due_reminders(&storage, now).unwrap_or_default();
    if due.is_empty() {
        return crate::AppCommandMsg::ReminderCheckFinished { newly_fired: Vec::new() };
    }

    let notifier = DbusNotifier::new();
    for reminder in &due {
        if let Err(err) = record_fired(&storage, reminder) {
            tracing::warn!(%err, event_id = reminder.event_id, "failed to record fired reminder");
            continue;
        }
        if settings.desktop_notifications_enabled {
            let request = NotifyRequest {
                app_name: "Calendarchy".into(),
                summary: reminder.event_title.clone(),
                body: format_reminder_body(reminder),
                urgency: Urgency::Normal,
                glyph: Some("\u{f0133}".into()), // nf-md-calendar
            };
            if let Err(err) = notifier.notify(request).await {
                tracing::warn!(%err, event_id = reminder.event_id, "failed to send desktop notification");
            }
        }
    }

    if settings.play_notification_sounds {
        play_sound(settings.notification_sound_path.clone());
    }

    crate::AppCommandMsg::ReminderCheckFinished { newly_fired: due }
}

fn format_reminder_body(reminder: &DueReminder) -> String {
    match reminder.reminder_minutes {
        0 => "Starting now".into(),
        1 => "Starting in 1 minute".into(),
        n => format!("Starting in {n} minutes"),
    }
}

/// Plays the reminder chime on its own detached thread — `rodio` needs no async
/// runtime, so a plain thread keeps this off both the GTK thread and the scheduler's
/// async command. Resolves a user-picked custom sound file first, else falls back to
/// `DEFAULT_SOUND_PATH`; a missing device/file/decoder is logged, never panics — a
/// failed chime shouldn't take down the reminder itself.
pub fn play_sound(custom_path: Option<String>) {
    std::thread::spawn(move || {
        let resolved = custom_path
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .or_else(|| {
                let default = PathBuf::from(DEFAULT_SOUND_PATH);
                default.exists().then_some(default)
            });
        let Some(path) = resolved else {
            tracing::debug!("no notification sound file available; skipping");
            return;
        };
        if let Err(err) = play_sound_file(&path) {
            tracing::warn!(%err, path = %path.display(), "failed to play notification sound");
        }
    });
}

fn play_sound_file(path: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let sink = rodio::stream::DeviceSinkBuilder::open_default_sink()?;
    // `rodio::stream::play` decodes internally — it takes the raw `Read + Seek`
    // reader, not a pre-built `Decoder`.
    let player = rodio::stream::play(sink.mixer(), std::io::BufReader::new(file))?;
    player.sleep_until_end();
    Ok(())
}

/// The floating multi-alert dialog (DESIGN_SPEC.md's roadmap phase 4 plus this
/// feature's own ask): a persistent singleton, built lazily on the first reminder and
/// then shown/hidden/rebuilt in place rather than re-created per alert. Cheap to
/// clone — every field is a GObject reference-counted handle, `Storage`, or
/// `ComponentSender`, same as `EventCtx`/`SettingsCtx` elsewhere in this app.
#[derive(Clone)]
pub struct OverlayHandle {
    window: gtk4::Window,
    bulk_actions_box: gtk4::Box,
    cards_box: gtk4::Box,
    history_box: gtk4::Box,
    storage: Storage,
    sender: ComponentSender<App>,
}

/// Finds or lazily builds the floating dialog. The window is created once and lives
/// for the rest of the app's life, same as the main window itself.
pub fn ensure_overlay(overlay: &Rc<RefCell<Option<OverlayHandle>>>, storage: &Storage, sender: &ComponentSender<App>) -> OverlayHandle {
    if let Some(handle) = overlay.borrow().as_ref() {
        return handle.clone();
    }
    let handle = build_overlay_window(storage.clone(), sender.clone());
    *overlay.borrow_mut() = Some(handle.clone());
    handle
}

fn build_overlay_window(storage: Storage, sender: ComponentSender<App>) -> OverlayHandle {
    let window = gtk4::Window::new();
    window.set_title(Some("Calendarchy reminders"));
    window.set_decorated(false);
    window.set_resizable(false);
    window.add_css_class("notification-overlay");

    let layer_shell_supported = gtk4_layer_shell::is_supported();
    if layer_shell_supported {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Right, true);
        window.set_keyboard_mode(KeyboardMode::OnDemand);
        window.set_namespace(Some("calendarchy-reminders"));
    } else {
        tracing::warn!(
            "gtk4-layer-shell isn't supported on this session (not a wlr-layer-shell \
             compositor) — the floating notification dialog will still work but won't \
             remember a screen position"
        );
    }

    let settings = load_settings(&storage).unwrap_or_default();
    let (margin_top, margin_right) = settings
        .notification_dialog_position
        .unwrap_or((DEFAULT_MARGIN_TOP, DEFAULT_MARGIN_RIGHT));
    if layer_shell_supported {
        window.set_margin(Edge::Top, margin_top);
        window.set_margin(Edge::Right, margin_right);
    }

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    root.add_css_class("notification-overlay-root");
    root.set_width_request(300);
    root.set_margin_all(10);

    // The header row doubles as the whole dialog's drag handle — dragging it
    // live-updates the layer-shell margins and persists the result on release, which
    // is what "floats and remembers position" resolves to without free x/y placement
    // (Wayland's xdg-shell forbids that; layer-shell only offers edge-anchored
    // margins).
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    header.set_cursor_from_name(Some("grab"));
    let header_label = gtk4::Label::new(Some("Reminders"));
    header_label.add_css_class("title-4");
    header_label.set_hexpand(true);
    header_label.set_halign(gtk4::Align::Start);
    header.append(&header_label);
    root.append(&header);

    if layer_shell_supported {
        header.add_controller(drag_controller(&window, &storage));
    }

    // Holds "Snooze all" / "Dismiss all" once 2+ alerts are active — empty and
    // hidden otherwise, since bulk actions don't mean anything below that.
    // Rebuilt fresh on every `refresh_overlay` call (see `bulk_actions_row`), not
    // built once here, so its Snooze-all button's label always reflects the current
    // `AppSettings::default_snooze_minutes`.
    let bulk_actions_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    root.append(&bulk_actions_box);

    let cards_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    root.append(&cards_box);

    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let history_toggle = gtk4::ToggleButton::with_label("Past");
    history_toggle.add_css_class("flat");
    history_toggle.set_halign(gtk4::Align::Start);
    root.append(&history_toggle);

    let history_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let history_revealer = gtk4::Revealer::new();
    history_revealer.set_child(Some(&history_box));
    history_revealer.set_reveal_child(false);
    root.append(&history_revealer);
    {
        let history_revealer = history_revealer.clone();
        history_toggle.connect_toggled(move |btn| history_revealer.set_reveal_child(btn.is_active()));
    }

    window.set_child(Some(&root));
    window.set_visible(false);

    OverlayHandle {
        window,
        bulk_actions_box,
        cards_box,
        history_box,
        storage,
        sender,
    }
}

/// Wires the header's `GestureDrag`: `drag-update` fires with `offset_x`/`offset_y`
/// cumulative since `drag-begin`, measured in the window's own surface-local
/// coordinates — since we're continuously repositioning that very window, reapplying
/// the full cumulative offset to a frozen drag-start snapshot each tick would measure
/// against a reference frame that has itself already moved, causing the dialog to lag
/// behind the pointer. Instead we track the incremental delta since the *previous*
/// tick and nudge the window's *current* live margin by just that much — immune to
/// the moving-reference-frame issue, and it also naturally respects whatever
/// `clamp_margin` (screen-edge clamping) did on the previous tick. `drag-end` persists
/// the final position via `save_settings`.
fn drag_controller(window: &gtk4::Window, storage: &Storage) -> gtk4::GestureDrag {
    let drag = gtk4::GestureDrag::new();
    let last_offset = Rc::new(Cell::new((0.0, 0.0)));

    {
        let last_offset = last_offset.clone();
        drag.connect_drag_begin(move |_, _, _| {
            last_offset.set((0.0, 0.0));
        });
    }
    {
        let window = window.clone();
        let last_offset = last_offset.clone();
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            let (last_x, last_y) = last_offset.get();
            let (dx, dy) = (offset_x - last_x, offset_y - last_y);
            last_offset.set((offset_x, offset_y));

            let new_top = clamp_margin(window.margin(Edge::Top) + dy as i32, &window, Edge::Top);
            let new_right = clamp_margin(window.margin(Edge::Right) - dx as i32, &window, Edge::Right);
            window.set_margin(Edge::Top, new_top);
            window.set_margin(Edge::Right, new_right);
        });
    }
    {
        let window = window.clone();
        let storage = storage.clone();
        drag.connect_drag_end(move |_, _, _| {
            let position = (window.margin(Edge::Top), window.margin(Edge::Right));
            let mut settings = load_settings(&storage).unwrap_or_default();
            settings.notification_dialog_position = Some(position);
            if let Err(err) = save_settings(&storage, &settings) {
                tracing::warn!(%err, "failed to save notification dialog position");
            }
        });
    }
    drag
}

/// Clamps a margin so the dialog can't be dragged past the opposite edge of its
/// current monitor. Falls back to "no clamp" if the monitor can't be resolved (e.g.
/// the window isn't mapped yet) — permissive rather than blocking the drag.
fn clamp_margin(value: i32, window: &gtk4::Window, edge: Edge) -> i32 {
    let monitor_size = window
        .surface()
        .and_then(|surface| gtk4::prelude::WidgetExt::display(window).monitor_at_surface(&surface))
        .map(|monitor| monitor.geometry());
    let max = match (edge, monitor_size) {
        (Edge::Top, Some(rect)) => (rect.height() - window.height().max(1)).max(0),
        (Edge::Right, Some(rect)) => (rect.width() - window.width().max(1)).max(0),
        _ => i32::MAX,
    };
    value.clamp(0, max)
}

/// Rebuilds the floating dialog's active-card list and "Past" history section from
/// storage — the same "clear and repopulate" convention `populate_sidebar` already
/// uses, so add/dismiss/snooze are all just "mutate storage, rebuild" instead of
/// patching individual rows in place. Shows the window if there's anything to show
/// (active or history), hides it otherwise rather than leaving an empty floating box
/// on screen.
pub fn refresh_overlay(handle: &OverlayHandle) {
    let settings = load_settings(&handle.storage).unwrap_or_default();
    let time_format = crate::resolve_time_format(&settings);

    let active = active_reminder_notifications(&handle.storage).unwrap_or_default();
    crate::clear_children(&handle.cards_box);
    for reminder in &active {
        handle
            .cards_box
            .append(&alert_card(reminder, time_format, settings.default_snooze_minutes, &handle.sender));
    }

    crate::clear_children(&handle.bulk_actions_box);
    handle.bulk_actions_box.set_visible(active.len() >= 2);
    if active.len() >= 2 {
        handle
            .bulk_actions_box
            .append(&bulk_actions_row(settings.default_snooze_minutes, &handle.sender));
    }

    let history = recent_reminder_history(&handle.storage, HISTORY_LIMIT).unwrap_or_default();
    crate::clear_children(&handle.history_box);
    for entry in &history {
        handle.history_box.append(&history_row(entry, time_format, &handle.sender));
    }

    handle.window.set_visible(!active.is_empty() || !history.is_empty());
}

/// Preset durations offered by the Snooze split-button's dropdown — common choices
/// spanning "a few minutes" through "a few hours"; the button's own default (picked
/// in Preferences, `AppSettings::default_snooze_minutes`) doesn't have to be one of
/// these.
const SNOOZE_PRESETS: &[(i64, &str)] = &[
    (5, "5 minutes"),
    (10, "10 minutes"),
    (15, "15 minutes"),
    (30, "30 minutes"),
    (60, "1 hour"),
    (180, "3 hours"),
];

fn alert_card(reminder: &ActiveReminder, time_format: TimeFormat, default_snooze_minutes: i64, sender: &ComponentSender<App>) -> gtk4::Box {
    let card = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    card.add_css_class("notification-card");

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let title = gtk4::Label::new(Some(&reminder.event_title));
    title.set_halign(gtk4::Align::Start);
    title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    text.append(&title);
    let time_label = gtk4::Label::new(Some(&format_event_start(&reminder.event_start, time_format)));
    time_label.add_css_class("dim-label");
    time_label.set_halign(gtk4::Align::Start);
    text.append(&time_label);
    card.append(&text);

    // A linked split button: the main button snoozes for the Preferences-configured
    // default straight away, and the dropdown arrow next to it opens a menu of other
    // durations for this one alert — without touching the stored default.
    let snooze_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    snooze_box.add_css_class("linked");

    let (snooze_quantity, snooze_unit_index) = crate::minutes_to_quantity_unit(default_snooze_minutes, &crate::SNOOZE_UNITS);
    let snooze_button = gtk4::Button::with_label(&format!(
        "Snooze {snooze_quantity}{}",
        crate::SNOOZE_UNITS[snooze_unit_index as usize].0
    ));
    {
        let sender = sender.clone();
        let id = reminder.id;
        snooze_button.connect_clicked(move |_| {
            sender.input(AppMsg::SnoozeReminder {
                id,
                minutes: default_snooze_minutes,
            });
        });
    }
    snooze_box.append(&snooze_button);
    let id = reminder.id;
    snooze_box.append(&snooze_presets_menu_button(sender, move |minutes| AppMsg::SnoozeReminder { id, minutes }));
    card.append(&snooze_box);

    let dismiss_button = gtk4::Button::from_icon_name("window-close-symbolic");
    dismiss_button.add_css_class("flat");
    dismiss_button.add_css_class("circular");
    dismiss_button.set_tooltip_text(Some("Dismiss"));
    {
        let sender = sender.clone();
        let id = reminder.id;
        dismiss_button.connect_clicked(move |_| sender.input(AppMsg::DismissReminder(id)));
    }
    card.append(&dismiss_button);

    card
}

/// A Snooze split-button's dropdown arrow — a `MenuButton` with no label, popping up
/// `SNOOZE_PRESETS` as a column of flat buttons. `to_msg` turns a picked preset's
/// minutes into the `AppMsg` to send — `SnoozeReminder` for one alert's card,
/// `SnoozeAllReminders` for the bulk-actions row — so this one popover builder serves
/// both without duplicating the popover-construction code. Picking a preset closes
/// the popover.
fn snooze_presets_menu_button(sender: &ComponentSender<App>, to_msg: impl Fn(i64) -> AppMsg + Clone + 'static) -> gtk4::MenuButton {
    let menu_button = gtk4::MenuButton::new();
    menu_button.set_icon_name("pan-down-symbolic");
    menu_button.set_tooltip_text(Some("Snooze for…"));

    let popover = gtk4::Popover::new();
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    list.set_margin_all(4);
    for &(minutes, label) in SNOOZE_PRESETS {
        let button = gtk4::Button::with_label(label);
        button.add_css_class("flat");
        let sender = sender.clone();
        let popover_weak = popover.downgrade();
        let to_msg = to_msg.clone();
        button.connect_clicked(move |_| {
            sender.input(to_msg(minutes));
            if let Some(popover) = popover_weak.upgrade() {
                popover.popdown();
            }
        });
        list.append(&button);
    }
    popover.set_child(Some(&list));
    menu_button.set_popover(Some(&popover));

    menu_button
}

/// The "Snooze all" / "Dismiss all" row shown once 2+ alerts are active
/// (`refresh_overlay` hides `OverlayHandle::bulk_actions_box` below that count).
/// "Snooze all" is a linked split button shaped exactly like a card's own Snooze
/// control — main button applies `default_snooze_minutes` to every active alert,
/// its dropdown offers `SNOOZE_PRESETS` instead.
fn bulk_actions_row(default_snooze_minutes: i64, sender: &ComponentSender<App>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    row.set_halign(gtk4::Align::End);

    let snooze_all_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    snooze_all_box.add_css_class("linked");

    let (quantity, unit_index) = crate::minutes_to_quantity_unit(default_snooze_minutes, &crate::SNOOZE_UNITS);
    let snooze_all_button = gtk4::Button::with_label(&format!("Snooze all {quantity}{}", crate::SNOOZE_UNITS[unit_index as usize].0));
    {
        let sender = sender.clone();
        snooze_all_button.connect_clicked(move |_| sender.input(AppMsg::SnoozeAllReminders(default_snooze_minutes)));
    }
    snooze_all_box.append(&snooze_all_button);
    snooze_all_box.append(&snooze_presets_menu_button(sender, AppMsg::SnoozeAllReminders));
    row.append(&snooze_all_box);

    let dismiss_all_button = gtk4::Button::with_label("Dismiss all");
    dismiss_all_button.add_css_class("flat");
    {
        let sender = sender.clone();
        dismiss_all_button.connect_clicked(move |_| sender.input(AppMsg::DismissAllReminders));
    }
    row.append(&dismiss_all_button);

    row
}

fn history_row(entry: &ReminderHistoryEntry, time_format: TimeFormat, sender: &ComponentSender<App>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.add_css_class("notification-history-row");

    let label = gtk4::Label::new(Some(&format!(
        "{} — {}",
        entry.event_title,
        format_event_start(&entry.event_start, time_format)
    )));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&label);

    let clear_button = gtk4::Button::from_icon_name("edit-clear-symbolic");
    clear_button.add_css_class("flat");
    clear_button.add_css_class("circular");
    clear_button.set_tooltip_text(Some("Clear"));
    {
        let sender = sender.clone();
        let id = entry.id;
        clear_button.connect_clicked(move |_| sender.input(AppMsg::ClearHistoryEntry(id)));
    }
    row.append(&clear_button);

    row
}

fn format_event_start(start: &str, time_format: TimeFormat) -> String {
    DateTime::parse_from_rfc3339(start)
        .map(|dt| crate::format_clock(dt.naive_local().time(), time_format))
        .unwrap_or_else(|_| start.to_string())
}

/// `AppMsg::SnoozeReminder`'s handler resolves `until` through this before calling
/// `snooze_reminder` — a plain "snooze for `minutes` from right now" duration, either
/// the Preferences-configured default (`AppSettings::default_snooze_minutes`, the
/// card's main Snooze button) or a preset the dropdown's menu picked instead.
pub fn snooze_until(minutes: i64) -> DateTime<Utc> {
    Utc::now() + ChronoDuration::minutes(minutes)
}
