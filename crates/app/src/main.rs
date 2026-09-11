use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use calendarchy_core::{
    load_settings, save_settings, Account, AccountId, AccountManager, AppSettings, DateFormat, EventEditorPanelMode,
    InvitationAutoAdd, Keyring, SecretServiceKeyring, ServiceKind, ServiceRegistry, Storage, TimeFormat,
};
use calendarchy_service_calendar::query::{
    calendars_by_account, clear_calendar_cache, create_event, delete_event, delete_reminder_notification,
    dismiss_all_active, dismiss_reminder, event_detail, events_for_visible_calendars, set_calendar_color,
    set_calendar_visibility, show_all_calendars, show_all_calendars_for_account, show_only_calendar,
    snooze_all_active, snooze_reminder, update_event, AttendeeResponseStatus, CalendarSummary, DisplayEvent,
    DueReminder, EventAttendeeInfo, EventBusyStatus, EventDetail, EventEdits, EventReminder, EventVisibility,
    ReminderMethod,
};
use calendarchy_service_calendar::recurrence::{self, Recurrence, RecurrenceEnd};
use calendarchy_service_calendar::CalendarService;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use gtk4::prelude::*;
use relm4::adw;
use relm4::adw::prelude::*;
use relm4::prelude::*;

mod notifications;

/// Core wiring (DESIGN_SPEC.md §6, roadmap phase 0) plumbed into the app so the UI
/// layer never talks to storage/services directly — it goes through this handle.
struct AppCore {
    accounts: AccountManager,
    storage: Storage,
}

/// `$XDG_DATA_HOME/calendarchy` (falling back to `~/.local/share/calendarchy`),
/// created if missing. This is where the local SQLite cache lives — DESIGN_SPEC.md's
/// whole offline-first design (§5/§9) depends on this surviving between launches,
/// unlike the in-memory database the app started with during early scaffolding.
fn data_dir() -> anyhow::Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map_err(|_| anyhow::anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    let dir = base.join("calendarchy");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn init_core() -> anyhow::Result<AppCore> {
    let db_path = data_dir()?.join("calendarchy.db");
    tracing::info!(path = %db_path.display(), "opening local database");
    let storage = Storage::open(&db_path)?;

    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(CalendarService::new()));
    let registry = Arc::new(registry);

    storage.with_conn(|conn| registry.migrate_all(conn))?;

    // Constructed on a short-lived runtime before the GLib main loop starts (rather
    // than inside it) to avoid nesting a tokio runtime inside relm4's own executor.
    let keyring: Arc<dyn Keyring> = {
        let rt = tokio::runtime::Runtime::new()?;
        Arc::new(rt.block_on(SecretServiceKeyring::new())?)
    };

    let accounts = AccountManager::new(storage.clone(), registry, keyring);

    Ok(AppCore { accounts, storage })
}

/// Which main-content view is displayed (DESIGN_SPEC.md §10). `Month`, `Day`, and
/// `FiveDay` are wired up — Schedule/Week/Year stay "coming soon" in the header bar's
/// view-switcher popover (`build_view_switcher_popover`) per the phased roadmap (§19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Month,
    Day,
    /// A rolling 5-day window centered on `App::current_date` (2 days before, the
    /// anchor, 2 days after — see `five_day_window`), not a fixed Mon–Fri work week.
    FiveDay,
    /// A side-by-side view of whatever dates the sidebar's mini calendar drag-select
    /// last produced (`App::custom_view_dates`) — entered only via
    /// `AppMsg::SetCustomViewDates`, never from the view-switcher popover.
    Custom,
}

/// Floor on the sidebar's drag-resized width — enforced by GTK itself via the
/// sidebar `ScrolledWindow`'s `width_request` combined with `shrink_start_child:
/// false` on the `Paned`, so a drag simply can't go narrower than this.
const SIDEBAR_MIN_WIDTH_PX: i32 = 180;
/// Ceiling on the sidebar's drag-resized width. Unlike the floor, GTK's `Paned` has
/// no built-in maximum (its `max_position` is derived from the end child's minimum
/// size, which is effectively 0), so this is enforced manually in the
/// `notify::position` handler set up in `init`.
const SIDEBAR_MAX_WIDTH_PX: i32 = 480;
/// Sidebar width used before the user has ever dragged it (`AppSettings::sidebar_width_fraction`
/// is `None`) — matches the fixed width the sidebar had before it became resizable.
const DEFAULT_SIDEBAR_WIDTH_PX: i32 = 240;

/// Floor/ceiling/default for the right-docked event editor panel's drag-resized
/// width, same enforcement shape as `SIDEBAR_MIN_WIDTH_PX`/`SIDEBAR_MAX_WIDTH_PX`
/// above. The default matches the old fixed-width edit dialog's `default_width(560)`.
const EDITOR_RIGHT_DOCK_MIN_WIDTH_PX: i32 = 320;
const EDITOR_RIGHT_DOCK_MAX_WIDTH_PX: i32 = 900;
const DEFAULT_EDITOR_RIGHT_DOCK_WIDTH_PX: i32 = 560;
/// Floor/ceiling/default for the bottom-docked event editor panel's drag-resized
/// height.
const EDITOR_BOTTOM_DOCK_MIN_HEIGHT_PX: i32 = 220;
const EDITOR_BOTTOM_DOCK_MAX_HEIGHT_PX: i32 = 700;
const DEFAULT_EDITOR_BOTTOM_DOCK_HEIGHT_PX: i32 = 360;
/// Default floating position (top-left margin, in px against a typical-size overlay)
/// used before the user has ever dragged the panel.
const DEFAULT_EDITOR_FLOATING_MARGIN_PX: (i32, i32) = (80, 80);
/// Fixed size of the floating event editor panel — unlike the two docked modes, the
/// user only asked to resize the panel *while snapped to an edge*, so floating keeps
/// one constant size (close to the old fixed-width dialog's own proportions) rather
/// than being independently resizable and persisted.
const DEFAULT_EDITOR_FLOATING_SIZE_PX: (i32, i32) = (560, 640);
/// While dragging, the panel's logical (floating-sized) position is considered
/// "close enough" to the content area's bottom/right edge to preview/resolve as
/// that dock once its distance to that edge drops under this. The same threshold
/// governs both snapping in and un-docking — `install_panel_drag` recomputes
/// which mode applies from scratch on every tick, purely from distance-to-edge,
/// rather than tracking "docked" and "floating" as separate branches each with
/// their own threshold (an earlier version of this constant did that, with a
/// deliberately larger un-dock threshold to avoid flicker right at the boundary;
/// that's no longer needed for a drag that *starts* docked, since it shows no
/// live preview at all until `drag-end` resolves it — see that function's doc
/// comment — though a drag that starts floating can still flicker its preview for
/// an instant if held exactly on this boundary, a minor enough case not to
/// warrant reintroducing a second threshold for it).
const EDITOR_PANEL_SNAP_THRESHOLD_PX: f64 = 28.0;

struct App {
    core: AppCore,
    /// Which main-content view is currently displayed. Switched via the header bar's
    /// view-switcher popover (`AppMsg::SetView`).
    current_view: ViewMode,
    /// Any date within the month currently shown in the grid — navigated by
    /// `Today`/`PrevMonth`/`NextMonth`, independent of the real calendar date used
    /// for the today-badge (`populate_month_grid`'s separate `today` argument). In
    /// `ViewMode::Day`, this instead identifies the exact day shown (paged by
    /// `AppMsg::PrevPeriod`/`NextPeriod`, one day at a time rather than one month). In
    /// `ViewMode::FiveDay`, this is the *anchor* the 5-day window is centered on
    /// (`five_day_window`), not a stored range — paged ±5 days/weekdays at a time. In
    /// `ViewMode::Custom`, this tracks `custom_view_dates[0]` (kept in sync on every
    /// selection/page) so the mini calendar and window title stay aligned with it.
    current_date: NaiveDate,
    /// The dates shown side by side in `ViewMode::Custom` — set by a mini calendar
    /// drag-select (`AppMsg::SetCustomViewDates`) and paged as a whole block by
    /// `AppMsg::PrevPeriod`/`NextPeriod`. Empty until the first drag-select; meaningless
    /// outside `ViewMode::Custom`.
    custom_view_dates: Vec<NaiveDate>,
    /// Live text from the header bar's search popover; re-applied on every refresh
    /// so it survives month navigation instead of resetting.
    search_query: String,
    /// Whether the left sidebar pane is currently shown — flipped by the header
    /// bar's hamburger toggle, independent of any individual calendar's visibility.
    sidebar_visible: bool,
    /// The floating multi-alert notification dialog (this feature) — `None` until the
    /// first reminder fires, then a persistent singleton for the rest of the app's
    /// life. See `notifications::ensure_overlay`.
    overlay: Rc<RefCell<Option<notifications::OverlayHandle>>>,
    /// The event editor panel currently open, if any — see `OpenEditorPanel`. Created
    /// once here and cloned (the `Rc`, not the contents) into every `EditorDocks`
    /// built for an `EventCtx`, so the one persistent drag gesture installed on
    /// `editor_overlay` at `init` time always sees the same cell regardless of which
    /// `EditorDocks` instance most recently populated it.
    open_editor_panel: Rc<RefCell<Option<OpenEditorPanel>>>,
}

#[derive(Debug)]
enum AppMsg {
    ToggleCalendarVisibility { calendar_id: i64, visible: bool },
    SetCalendarColor { calendar_id: i64, color: String },
    ShowOnlyCalendar { calendar_id: i64 },
    ShowAllCalendars,
    ShowAllCalendarsForAccount { account_id: AccountId },
    Today,
    /// Always pages the mini calendar's own month navigator by a whole month,
    /// regardless of which main view is active (DESIGN_SPEC.md §10) — the main
    /// content toolbar's Prev/Next arrows send `PrevPeriod`/`NextPeriod` instead,
    /// which page by whatever unit the active view uses.
    PrevMonth,
    NextMonth,
    /// The main content toolbar's Prev/Next arrows: one day in `ViewMode::Day`, one
    /// month in `ViewMode::Month` (matching `PrevMonth`/`NextMonth`'s old behavior).
    PrevPeriod,
    NextPeriod,
    SetView(ViewMode),
    /// Ctrl+scroll on the Day view grid (`install_day_zoom_controller`): steps
    /// `AppSettings::day_time_scale_minutes` one increment finer (`true`) or coarser
    /// (`false`) via `cycle_day_time_scale`, persists it, and re-renders.
    ZoomDayTimeScale { finer: bool },
    SearchChanged(String),
    ShowShortcuts,
    Resize,
    JumpToDate(NaiveDate),
    /// A completed drag-select on the sidebar's mini calendar (`populate_mini_calendar`'s
    /// `on_range_pick`) — always 2+ consecutive dates, capped at
    /// `MINI_CALENDAR_DRAG_MAX_DAYS`. Switches to `ViewMode::Custom` showing exactly
    /// these dates side by side.
    SetCustomViewDates(Vec<NaiveDate>),
    EventUpdated,
    ToggleSidebar,
    CreateEvent,
    ShowPreferences,
    /// A card's Snooze button (`minutes` = `AppSettings::default_snooze_minutes`) or
    /// its dropdown's preset picker (`minutes` = whichever preset was clicked) in the
    /// floating notification dialog.
    SnoozeReminder { id: i64, minutes: i64 },
    /// A card's Dismiss button, or a "Past" row's Clear button on an active alert —
    /// both just flip one row to `dismissed` (DESIGN_SPEC.md §13).
    DismissReminder(i64),
    /// The floating dialog's "Dismiss all" button.
    DismissAllReminders,
    /// The floating dialog's "Snooze all" split button (shown alongside "Dismiss all"
    /// once 2+ alerts are active) — same `minutes` shape as `SnoozeReminder`, applied
    /// to every currently active alert at once.
    SnoozeAllReminders(i64),
    /// A "Past" history row's Clear button — removes that row for good, distinct from
    /// `DismissReminder` (active → dismissed, which is how a row *becomes* history).
    ClearHistoryEntry(i64),
}

/// Results of a background command spawned from the Preferences window's "Sync now"
/// / "Clear local cache and resync" actions (DESIGN_SPEC.md §12) — routed through
/// `Component::CommandOutput` (relm4's async-command channel) rather than a `std`
/// thread + manual `sender.input`, since `Service::sync`/`on_enabled` are already
/// `async` and `AccountManager` is cheap to clone onto relm4's own runtime.
#[derive(Debug)]
enum AppCommandMsg {
    SyncFinished { synced: usize, failed: usize },
    CacheCleared { cleared: usize, failed: usize },
    /// One Notification Scheduler poll tick finished (`notifications::start_scheduler`,
    /// DESIGN_SPEC.md §13) — empty when nothing was due. Recording, OS-toast delivery,
    /// and sound playback have already happened by the time this arrives; the GTK
    /// thread's only job left is refreshing the floating dialog if anything's new.
    ReminderCheckFinished { newly_fired: Vec<DueReminder> },
}

/// The event editor panel currently open, if any, and just enough about it for the
/// one persistent drag gesture (`install_panel_drag`, attached once at `init` to
/// `editor_overlay` — deliberately never to the panel itself, see that function's
/// doc comment) to know what to drag and where it currently is. Populated by
/// `show_edit_event_dialog` right after the panel's initial `dock_panel` attach,
/// and cleared by `close_panel`. `relayout` re-flows the panel's own field rows for
/// a newly-committed mode (see `relayout_editor_body`) — called once up front for
/// the panel's starting mode, and again by `install_panel_drag`'s `drag-end` every
/// time a live drag actually commits to a new mode.
#[derive(Clone)]
struct OpenEditorPanel {
    root: adw::ToolbarView,
    titlebar: gtk4::Box,
    mode: Rc<Cell<EventEditorPanelMode>>,
    relayout: Rc<dyn Fn(EventEditorPanelMode)>,
}

/// The main window's editor-docking widgets (DESIGN_SPEC.md §10's redesigned
/// floating/bottom/right dockable event editor) — cheap GObject-handle clones, same
/// "everything a dialog needs, cloned once" shape as `EventCtx` itself. `overlay` is
/// the content-area `gtk4::Overlay` the floating panel is added into/removed from at
/// runtime (`editor_overlay` in the `view!` macro); `right_paned`/`bottom_paned` are
/// the two dock `Paned`s whose `notify::position` drives resize persistence;
/// `right_slot`/`bottom_slot` are the (normally hidden) `Box`es the panel's root is
/// reparented into while docked. `main_content_box`'s shared-edge margin is zeroed
/// while a dock is active and restored when not (`update_dock_margins`) — the
/// sidebar sits outside both docks' `Paned`s entirely and never needs that, but
/// `sidebar_pane`'s own current width is still read by `preview_docked_bottom` to
/// keep the bottom dock's live drag-preview from covering it. `current_panel` is shared
/// (an `Rc<RefCell<...>>`, cloned from a single instance stored on `App` — see
/// `install_panel_drag`) rather than rebuilt fresh in each `EditorDocks`, since it
/// has to be the *same* cell every time for the persistent drag gesture to see
/// what the freshly-(re)built `EditorDocks` in `AppMsg::CreateEvent`/`App::refresh`
/// populate into it.
#[derive(Clone)]
struct EditorDocks {
    overlay: gtk4::Overlay,
    right_paned: gtk4::Paned,
    right_slot: gtk4::Box,
    bottom_paned: gtk4::Paned,
    bottom_slot: gtk4::Box,
    sidebar_pane: gtk4::ScrolledWindow,
    main_content_box: gtk4::Box,
    current_panel: Rc<RefCell<Option<OpenEditorPanel>>>,
}

/// Everything an event chip's click handler and the edit dialog it opens need, bundled
/// so it can be threaded through `populate_month_grid`/`event_row` as one clone-able
/// value instead of three separate parameters. `window` is the app's single top-level
/// window, used as the edit dialog's transient parent. `date_format`/`time_format` are
/// DESIGN_SPEC.md §12's Language and region overrides, already resolved (never
/// `DateFormat::System`/`TimeFormat::System`, see `resolve_date_format`/
/// `resolve_time_format`) so every rendering function downstream just matches on a
/// concrete choice instead of re-deriving the system-locale fallback itself. `docks`
/// is what the event editor panel reparents itself into/out of across its three view
/// modes — see `EditorDocks`.
#[derive(Clone)]
struct EventCtx {
    storage: Storage,
    sender: ComponentSender<App>,
    window: adw::Window,
    date_format: DateFormat,
    time_format: TimeFormat,
    docks: EditorDocks,
}

/// Everything the Preferences window (DESIGN_SPEC.md §12) needs: `storage` for
/// reading/writing settings and listing calendars for the "Default calendar" picker,
/// `accounts` for the "Sync now" / "Clear local cache and resync" actions, `sender` to
/// refresh the main view once those finish, `window` as their dialogs' transient
/// parent, and `world_clock_box` so the World Clock group's rows can repaint the
/// sidebar's module immediately on every add/remove/reorder instead of waiting on the
/// periodic ticker (`start_world_clock_ticker`) — same "everything a dialog needs,
/// cloned once" shape as `EventCtx`.
#[derive(Clone)]
struct SettingsCtx {
    storage: Storage,
    accounts: AccountManager,
    sender: ComponentSender<App>,
    window: adw::Window,
    world_clock_box: gtk4::Box,
}

#[relm4::component]
impl Component for App {
    type Init = AppCore;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = AppCommandMsg;

    view! {
        adw::Window {
            set_title: Some("Calendarchy"),
            set_default_width: 1100,
            set_default_height: 720,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    pack_start = &gtk4::Button {
                        set_icon_name: "open-menu-symbolic",
                        add_css_class: "flat",
                        set_tooltip_text: Some("Toggle sidebar"),
                        connect_clicked => AppMsg::ToggleSidebar,
                    },

                    pack_start = &gtk4::Button {
                        set_label: "Create",
                        add_css_class: "pill",
                        add_css_class: "suggested-action",
                        connect_clicked => AppMsg::CreateEvent,
                    },

                    pack_start = &gtk4::Button {
                        set_label: "Today",
                        add_css_class: "pill",
                        connect_clicked => AppMsg::Today,
                    },

                    pack_start = &gtk4::Box {
                        add_css_class: "linked",
                        set_orientation: gtk4::Orientation::Horizontal,

                        gtk4::Button {
                            set_icon_name: "go-previous-symbolic",
                            set_tooltip_text: Some("Previous"),
                            connect_clicked => AppMsg::PrevPeriod,
                        },
                        gtk4::Button {
                            set_icon_name: "go-next-symbolic",
                            set_tooltip_text: Some("Next"),
                            connect_clicked => AppMsg::NextPeriod,
                        },
                    },

                    #[name = "window_title"]
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: &month_title,
                    },

                    #[name = "view_menu_button"]
                    pack_end = &gtk4::MenuButton {
                        set_label: "Month",
                        add_css_class: "pill",
                        add_css_class: "view-switcher-button",
                    },

                    pack_end = &gtk4::Button {
                        set_icon_name: "emblem-system-symbolic",
                        add_css_class: "flat",
                        set_tooltip_text: Some("Preferences (Ctrl+,)"),
                        connect_clicked => AppMsg::ShowPreferences,
                    },

                    pack_end = &gtk4::Button {
                        set_icon_name: "dialog-question-symbolic",
                        add_css_class: "flat",
                        set_tooltip_text: Some("Keyboard shortcuts"),
                        connect_clicked => AppMsg::ShowShortcuts,
                    },

                    #[name = "search_entry"]
                    pack_end = &gtk4::SearchEntry {
                        add_css_class: "search-field",
                        set_placeholder_text: Some("Search events"),
                        set_width_request: 220,
                        set_valign: gtk4::Align::Center,

                        connect_search_changed[sender] => move |entry| {
                            sender.input(AppMsg::SearchChanged(entry.text().to_string()));
                        },

                        connect_stop_search[sender] => move |entry| {
                            entry.set_text("");
                            sender.input(AppMsg::SearchChanged(String::new()));
                        },
                    },
                },

                #[name = "editor_overlay"]
                #[wrap(Some)]
                set_content = &gtk4::Overlay {
                    set_hexpand: true,
                    set_vexpand: true,

                    // The sidebar sits outside the bottom-dock split entirely (its own
                    // outermost Paned, wrapping everything to its right) so it always
                    // spans the full window height — bottom-docking the event editor
                    // only cuts into the main content column's height, never the
                    // sidebar's.
                    #[name = "sidebar_paned"]
                    gtk4::Paned {
                        set_orientation: gtk4::Orientation::Horizontal,
                        set_wide_handle: true,
                        set_resize_start_child: false,
                        set_resize_end_child: true,
                        set_shrink_start_child: false,
                        set_shrink_end_child: true,

                        #[name = "sidebar_pane"]
                        #[wrap(Some)]
                        set_start_child = &gtk4::ScrolledWindow {
                            add_css_class: "card",
                            set_overflow: gtk4::Overflow::Hidden,
                            set_margin_start: 12,
                            set_margin_top: 12,
                            set_margin_bottom: 12,
                            set_width_request: SIDEBAR_MIN_WIDTH_PX,
                            set_hexpand: false,
                            set_vexpand: true,
                            set_hscrollbar_policy: gtk4::PolicyType::Never,

                            gtk4::Box {
                                set_orientation: gtk4::Orientation::Vertical,
                                set_spacing: 12,
                                set_margin_all: 12,

                                gtk4::Box {
                                    add_css_class: "mini-calendar",
                                    set_orientation: gtk4::Orientation::Vertical,
                                    set_spacing: 6,

                                    gtk4::Box {
                                        set_orientation: gtk4::Orientation::Horizontal,
                                        set_spacing: 2,

                                        #[name = "mini_calendar_title"]
                                        gtk4::MenuButton {
                                            add_css_class: "flat",
                                            add_css_class: "mini-calendar-title",
                                            set_hexpand: true,
                                            set_halign: gtk4::Align::Start,
                                            set_tooltip_text: Some("Jump to month/year"),
                                        },
                                        gtk4::Button {
                                            set_icon_name: "go-previous-symbolic",
                                            add_css_class: "flat",
                                            set_tooltip_text: Some("Previous month"),
                                            connect_clicked => AppMsg::PrevMonth,
                                        },
                                        gtk4::Button {
                                            set_icon_name: "go-next-symbolic",
                                            add_css_class: "flat",
                                            set_tooltip_text: Some("Next month"),
                                            connect_clicked => AppMsg::NextMonth,
                                        },
                                    },

                                    #[name = "mini_calendar_grid"]
                                    gtk4::Grid {
                                        add_css_class: "mini-calendar-grid",
                                        set_row_homogeneous: true,
                                        set_column_homogeneous: true,
                                        set_row_spacing: 2,
                                        set_column_spacing: 2,
                                    },
                                },

                                #[name = "world_clock_box"]
                                gtk4::Box {
                                    add_css_class: "world-clock",
                                    set_orientation: gtk4::Orientation::Vertical,
                                    set_spacing: 4,
                                },

                                gtk4::Separator {},

                                #[name = "sidebar_list"]
                                gtk4::Box {
                                    add_css_class: "sidebar",
                                    set_orientation: gtk4::Orientation::Vertical,
                                    set_spacing: 2,
                                },
                            },
                        },

                        #[name = "right_dock_paned"]
                        #[wrap(Some)]
                        set_end_child = &gtk4::Paned {
                            set_orientation: gtk4::Orientation::Horizontal,
                            set_wide_handle: true,
                            set_resize_start_child: true,
                            set_resize_end_child: false,
                            set_shrink_start_child: true,
                            set_shrink_end_child: false,

                            #[name = "bottom_dock_paned"]
                            #[wrap(Some)]
                            set_start_child = &gtk4::Paned {
                                set_orientation: gtk4::Orientation::Vertical,
                                set_wide_handle: true,
                                set_resize_start_child: true,
                                set_resize_end_child: false,
                                set_shrink_start_child: true,
                                set_shrink_end_child: false,

                                #[name = "main_content_box"]
                                #[wrap(Some)]
                                set_start_child = &gtk4::Box {
                                    set_orientation: gtk4::Orientation::Vertical,
                                    set_hexpand: true,
                                    set_vexpand: true,
                                    set_spacing: 12,
                                    set_margin_top: 12,
                                    set_margin_bottom: 12,
                                    set_margin_end: 12,

                                    #[name = "month_view_container"]
                                    gtk4::ScrolledWindow {
                                        add_css_class: "card",
                                        set_overflow: gtk4::Overflow::Hidden,
                                        set_vexpand: true,
                                        set_hexpand: true,
                                        set_visible: true,

                                        #[name = "month_grid"]
                                        gtk4::Grid {
                                            add_css_class: "month-grid",
                                            set_row_homogeneous: true,
                                            set_column_homogeneous: true,
                                            set_hexpand: true,
                                            set_vexpand: true,
                                        },
                                    },

                                    #[name = "day_view_container"]
                                    gtk4::Box {
                                        add_css_class: "card",
                                        set_orientation: gtk4::Orientation::Vertical,
                                        set_overflow: gtk4::Overflow::Hidden,
                                        set_vexpand: true,
                                        set_hexpand: true,
                                        set_visible: false,

                                        #[name = "day_header_box"]
                                        gtk4::Box {
                                            set_orientation: gtk4::Orientation::Horizontal,
                                            add_css_class: "day-header-row",
                                        },

                                        #[name = "day_all_day_box"]
                                        gtk4::Grid {
                                            add_css_class: "day-all-day-strip",
                                            set_row_spacing: 2,
                                            set_column_spacing: 2,
                                            set_visible: false,
                                        },

                                        gtk4::Separator {},

                                        #[name = "day_scroller"]
                                        gtk4::ScrolledWindow {
                                            set_vexpand: true,
                                            set_hexpand: true,
                                            set_hscrollbar_policy: gtk4::PolicyType::Never,

                                            #[name = "day_overlay"]
                                            gtk4::Overlay {
                                                #[name = "day_hour_grid"]
                                                gtk4::Grid {
                                                    add_css_class: "day-hour-grid",
                                                    set_hexpand: true,
                                                },
                                            },
                                        },
                                    },
                                },

                                #[name = "bottom_dock_slot"]
                                #[wrap(Some)]
                                set_end_child = &gtk4::Box {
                                    add_css_class: "card",
                                    set_overflow: gtk4::Overflow::Hidden,
                                    set_hexpand: true,
                                    set_vexpand: true,
                                    set_margin_end: 12,
                                    set_margin_bottom: 12,
                                    set_visible: false,
                                },
                            },

                            #[name = "right_dock_slot"]
                            #[wrap(Some)]
                            set_end_child = &gtk4::Box {
                                add_css_class: "card",
                                set_overflow: gtk4::Overflow::Hidden,
                                set_hexpand: true,
                                set_vexpand: true,
                                set_margin_top: 12,
                                set_margin_bottom: 12,
                                set_margin_end: 12,
                                set_visible: false,
                            },
                        },
                    },
                },
            }
        }
    }

    fn init(core: Self::Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let today = Local::now().date_naive();
        let month_title = today.format("%B %Y").to_string();

        let accounts = core.accounts.list_accounts().unwrap_or_default();
        let calendars = calendars_by_account(&core.storage).unwrap_or_default();
        let events = events_for_visible_calendars(&core.storage).unwrap_or_default();
        tracing::info!(
            accounts = accounts.len(),
            calendars = calendars.len(),
            events = events.len(),
            "loaded state at startup"
        );

        load_static_css();
        load_calendar_color_css(&calendars);
        load_palette_color_css();

        // Read once at startup so the one setting with an immediate visual effect
        // (§12's view density option) is already applied before the first frame —
        // every other read of settings happens fresh from storage right where it's
        // needed (`AppMsg::CreateEvent`, `show_preferences_window`) rather than being
        // cached on the model, so a change made in Preferences can never go stale.
        let settings = load_settings(&core.storage).unwrap_or_default();
        apply_compact_density(&root, settings.compact_density);
        // No search query yet at startup, but "Show declined events" still applies —
        // see `filter_events`'s doc comment for why this half lives app-side.
        let events = filter_events(&events, "", settings.show_declined_events);

        // `Storage` is a cheap `Arc`-backed clone (calendarchy_core::Storage) — cloned
        // here, before `core` moves into `model` below, since `ctx` itself can only be
        // built after `view_output!()` produces `widgets` (its `docks` field needs the
        // editor-docking widgets that macro invocation generates).
        let storage_for_ctx = core.storage.clone();
        let open_editor_panel = Rc::new(RefCell::new(None));

        let model = App {
            core,
            current_view: ViewMode::Month,
            current_date: today,
            custom_view_dates: Vec::new(),
            search_query: String::new(),
            sidebar_visible: true,
            overlay: Rc::new(RefCell::new(None)),
            open_editor_panel: open_editor_panel.clone(),
        };
        let widgets = view_output!();

        // `root` (the app's one top-level window) is cloned here rather than
        // re-fetched later since nothing else needs an owned handle to it.
        let ctx = EventCtx {
            storage: storage_for_ctx,
            sender: sender.clone(),
            window: root.clone(),
            date_format: resolve_date_format(&settings),
            time_format: resolve_time_format(&settings),
            docks: EditorDocks {
                overlay: widgets.editor_overlay.clone(),
                right_paned: widgets.right_dock_paned.clone(),
                right_slot: widgets.right_dock_slot.clone(),
                bottom_paned: widgets.bottom_dock_paned.clone(),
                bottom_slot: widgets.bottom_dock_slot.clone(),
                sidebar_pane: widgets.sidebar_pane.clone(),
                main_content_box: widgets.main_content_box.clone(),
                current_panel: open_editor_panel,
            },
        };

        widgets.view_menu_button.set_popover(Some(&build_view_switcher_popover(&sender, &ctx.storage)));
        widgets.view_menu_button.set_label(view_mode_label(model.current_view));
        widgets.month_view_container.set_visible(model.current_view == ViewMode::Month);
        widgets.day_view_container.set_visible(matches!(model.current_view, ViewMode::Day | ViewMode::FiveDay));

        populate_month_grid(&widgets.month_grid, today, today, &events, &ctx);
        let gutter_px = populate_day_view(
            &widgets.day_overlay,
            &[today],
            today,
            &events,
            &ctx,
            settings.day_time_scale_minutes,
            settings.day_drag_snap_ctrl_minutes,
            settings.day_drag_snap_ctrl_shift_minutes,
            settings.day_drag_hold_ms,
        );
        populate_day_header(&widgets.day_header_box, &widgets.day_all_day_box, &[today], today, &events, &ctx, gutter_px);
        {
            // The sidebar's mini calendar isn't inside a popover of its own, so
            // there's nothing to accidentally close early here — jumping a month and
            // picking a day both just page the main view (`AppMsg::JumpToDate`),
            // unlike `date_picker`'s two callbacks, which mean different things.
            let jump = jump_to_date_callback(&sender);
            let range_pick = range_pick_callback(&sender);
            populate_mini_calendar(
                &widgets.mini_calendar_grid,
                &widgets.mini_calendar_title,
                today,
                today,
                &jump,
                &jump,
                Some(&range_pick),
                &[],
            );
        }
        populate_sidebar(&widgets.sidebar_list, &accounts, &calendars, &sender);
        populate_world_clock(&widgets.world_clock_box, &settings);
        install_search_shortcut(&root, &widgets.search_entry);
        install_preferences_shortcut(&root, &sender);
        install_view_shortcuts(&root, &sender);
        install_day_zoom_controller(&widgets.day_scroller, &sender);
        {
            let fraction = settings
                .sidebar_width_fraction
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH_PX as f64 / 1100.0);
            install_sidebar_resize_persistence(&widgets.sidebar_paned, &root, &ctx.storage, fraction);
        }
        // Unlike the sidebar above, these two attach *only* the debounced-save half of
        // the pattern — no startup restore, since the event editor panel (and thus
        // both dock `Paned`s' end children) isn't shown until a user opens it. The
        // panel's initial size is restored lazily, straight from settings, the first
        // time it actually docks in a session — see `dock_panel`.
        install_right_dock_resize_persistence(&widgets.right_dock_paned, &widgets.right_dock_slot, &ctx.storage);
        install_bottom_dock_resize_persistence(&widgets.bottom_dock_paned, &widgets.bottom_dock_slot, &ctx.storage);
        install_panel_drag(&widgets.editor_overlay, &ctx.docks, &ctx.storage);
        start_world_clock_ticker(&widgets.world_clock_box, ctx.storage.clone());
        notifications::start_scheduler(sender.clone(), ctx.storage.clone());
        // Shows any reminders still `active` from a previous run immediately, rather
        // than waiting for the first poll tick (`notifications::start_scheduler`) to
        // fire — the floating dialog should reflect state that already exists in
        // storage as soon as the app opens.
        {
            let handle = notifications::ensure_overlay(&model.overlay, &ctx.storage, &sender);
            notifications::refresh_overlay(&handle, &root);
        }

        // `populate_month_grid`/`populate_day_view` above ran before the window had a
        // real allocated size, so `compute_max_visible_events` fell back to a
        // conservative guess and the day view's event-column widths were computed off
        // an unrealistically small `day_overlay.width()`. See
        // `poll_for_real_width_then_resize`'s doc comment for why both need this.
        poll_for_real_width_then_resize(&widgets.month_grid, &sender);
        poll_for_real_width_then_resize(&widgets.day_overlay, &sender);

        // Re-run continuously while the window is actively being resized, so dragging
        // keeps the month grid's event count and the day view's event-column widths
        // matched to the space actually available. Also re-runs while the sidebar
        // Paned's handle is being dragged: that changes the main content's available
        // width without changing the window's own size, so `paned.position()` is
        // tracked alongside width/height rather than relying on a separate mechanism.
        //
        // Deliberately *not* `root.connect_notify_local(Some("default-width"/"default-height"), ...)`
        // (what this used to be): those properties are size *hints* for initial/remembered
        // window placement, not a live "the window is now this size" feed — in practice they
        // don't fire on every step of an interactive drag under this compositor, only
        // settling once the gesture ends (confirmed by testing: switching to Day view and
        // back to Month already forces a correct recompute via `refresh()`, so the grid-fit
        // math itself is fine — it's specifically the *during-the-drag* updates that were
        // missing). GTK4 also has no generic per-widget "size changed" signal to fall back
        // on, so instead this polls the window's own allocated size on the frame clock: a
        // tick callback fires once per frame for as long as *something* is requesting a
        // redraw, which an interactive resize does continuously (the compositor repaints the
        // window every step of the drag) and an idle window does not — so this stays a cheap
        // no-op integer comparison until a resize is actually happening. Registered once,
        // for the app's lifetime (never removed), matching this file's other
        // `timeout_add_local` tickers (e.g. `start_world_clock_ticker`) that are likewise
        // fire-and-forget rather than stored/cancelled.
        {
            let sender = sender.clone();
            let paned = widgets.sidebar_paned.clone();
            let last_state: Rc<Cell<(i32, i32, i32)>> = Rc::new(Cell::new((0, 0, 0)));
            root.add_tick_callback(move |window, _clock| {
                let state = (window.width(), window.height(), paned.position());
                if state != last_state.get() && state.0 > 0 && state.1 > 0 {
                    last_state.set(state);
                    sender.input(AppMsg::Resize);
                }
                gtk4::glib::ControlFlow::Continue
            });
        }

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            AppMsg::ToggleCalendarVisibility { calendar_id, visible } => {
                if let Err(err) = set_calendar_visibility(&self.core.storage, calendar_id, visible) {
                    tracing::warn!(%err, calendar_id, "failed to update calendar visibility");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::SetCalendarColor { calendar_id, color } => {
                if let Err(err) = set_calendar_color(&self.core.storage, calendar_id, &color) {
                    tracing::warn!(%err, calendar_id, "failed to update calendar color");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::ShowOnlyCalendar { calendar_id } => {
                if let Err(err) = show_only_calendar(&self.core.storage, calendar_id) {
                    tracing::warn!(%err, calendar_id, "failed to isolate calendar visibility");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::ShowAllCalendars => {
                if let Err(err) = show_all_calendars(&self.core.storage) {
                    tracing::warn!(%err, "failed to show all calendars");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::ShowAllCalendarsForAccount { account_id } => {
                if let Err(err) = show_all_calendars_for_account(&self.core.storage, account_id) {
                    tracing::warn!(%err, ?account_id, "failed to show all calendars for account");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::Today => {
                self.current_date = Local::now().date_naive();
                self.refresh(widgets, &sender, root);
            }
            AppMsg::PrevMonth => {
                self.current_date = shift_month(self.current_date, -1);
                self.refresh(widgets, &sender, root);
            }
            AppMsg::NextMonth => {
                self.current_date = shift_month(self.current_date, 1);
                self.refresh(widgets, &sender, root);
            }
            AppMsg::PrevPeriod => {
                match self.current_view {
                    ViewMode::Month => self.current_date = shift_month(self.current_date, -1),
                    ViewMode::Day => self.current_date -= Duration::days(1),
                    ViewMode::FiveDay => {
                        let show_weekends = load_settings(&self.core.storage).unwrap_or_default().show_weekends;
                        self.current_date = if show_weekends {
                            self.current_date - Duration::days(5)
                        } else {
                            step_weekdays(self.current_date, -5)
                        };
                    }
                    ViewMode::Custom => {
                        let span = self.custom_view_dates.len() as i64;
                        for date in &mut self.custom_view_dates {
                            *date -= Duration::days(span);
                        }
                        if let Some(&first) = self.custom_view_dates.first() {
                            self.current_date = first;
                        }
                    }
                };
                self.refresh(widgets, &sender, root);
            }
            AppMsg::NextPeriod => {
                match self.current_view {
                    ViewMode::Month => self.current_date = shift_month(self.current_date, 1),
                    ViewMode::Day => self.current_date += Duration::days(1),
                    ViewMode::FiveDay => {
                        let show_weekends = load_settings(&self.core.storage).unwrap_or_default().show_weekends;
                        self.current_date = if show_weekends {
                            self.current_date + Duration::days(5)
                        } else {
                            step_weekdays(self.current_date, 5)
                        };
                    }
                    ViewMode::Custom => {
                        let span = self.custom_view_dates.len() as i64;
                        for date in &mut self.custom_view_dates {
                            *date += Duration::days(span);
                        }
                        if let Some(&first) = self.custom_view_dates.first() {
                            self.current_date = first;
                        }
                    }
                };
                self.refresh(widgets, &sender, root);
            }
            AppMsg::SetView(view) => {
                self.current_view = view;
                widgets.month_view_container.set_visible(view == ViewMode::Month);
                widgets.day_view_container.set_visible(matches!(view, ViewMode::Day | ViewMode::FiveDay | ViewMode::Custom));
                widgets.view_menu_button.set_label(view_mode_label(view));
                self.refresh(widgets, &sender, root);
                if matches!(view, ViewMode::Day | ViewMode::FiveDay | ViewMode::Custom) {
                    // `day_overlay` was hidden (inside `day_view_container`) until the
                    // `set_visible(true)` above, so the `refresh` just above computed
                    // its event columns off a stale/zero width — correct it once a
                    // real one is allocated (see `poll_for_real_width_then_resize`).
                    poll_for_real_width_then_resize(&widgets.day_overlay, &sender);
                    let settings = load_settings(&self.core.storage).unwrap_or_default();
                    let today = Local::now().date_naive();
                    let today_visible = match view {
                        ViewMode::Day => self.current_date == today,
                        ViewMode::FiveDay => five_day_window(self.current_date, settings.show_weekends).contains(&today),
                        ViewMode::Custom => self.custom_view_dates.contains(&today),
                        ViewMode::Month => false,
                    };
                    if today_visible {
                        scroll_day_view_to_now(&widgets.day_scroller, settings.day_time_scale_minutes);
                    }
                }
            }
            AppMsg::SetCustomViewDates(dates) => {
                self.current_view = ViewMode::Custom;
                if let Some(&first) = dates.first() {
                    self.current_date = first;
                }
                self.custom_view_dates = dates;
                widgets.month_view_container.set_visible(false);
                widgets.day_view_container.set_visible(true);
                widgets.view_menu_button.set_label(view_mode_label(self.current_view));
                self.refresh(widgets, &sender, root);
                // Same reasoning as `SetView`'s Day/5-day branch above: `day_overlay`
                // was hidden until just now, so this refresh's column widths need a
                // follow-up pass once real geometry is allocated.
                poll_for_real_width_then_resize(&widgets.day_overlay, &sender);
                let today = Local::now().date_naive();
                if self.custom_view_dates.contains(&today) {
                    let settings = load_settings(&self.core.storage).unwrap_or_default();
                    scroll_day_view_to_now(&widgets.day_scroller, settings.day_time_scale_minutes);
                }
            }
            AppMsg::ZoomDayTimeScale { finer } => {
                let mut settings = load_settings(&self.core.storage).unwrap_or_default();
                settings.day_time_scale_minutes = cycle_day_time_scale(settings.day_time_scale_minutes, finer);
                if let Err(err) = save_settings(&self.core.storage, &settings) {
                    tracing::warn!(%err, "failed to save day time scale");
                }
                self.refresh(widgets, &sender, root);
            }
            AppMsg::SearchChanged(query) => {
                self.search_query = query;
                self.refresh(widgets, &sender, root);
            }
            AppMsg::ShowShortcuts => {
                show_shortcuts_window(root);
            }
            AppMsg::Resize => {
                let started = std::time::Instant::now();
                self.refresh(widgets, &sender, root);
                tracing::info!(elapsed_ms = started.elapsed().as_millis(), "DEBUG resize refresh done");
            }
            AppMsg::JumpToDate(date) => {
                // A plain click on the mini calendar while a drag-selected range is
                // showing (`ViewMode::Custom`) only actually changes anything on
                // screen if the clicked date falls outside that range — `Custom`'s
                // visible columns come from `custom_view_dates`, not `current_date`
                // (see `App::refresh`), so a click *inside* the range is a no-op here
                // beyond the cosmetic `current_date` sync below. A click outside it
                // drops out of the range into a plain single-day view instead.
                if self.current_view == ViewMode::Custom && !self.custom_view_dates.contains(&date) {
                    self.current_view = ViewMode::Day;
                    widgets.month_view_container.set_visible(false);
                    widgets.day_view_container.set_visible(true);
                    widgets.view_menu_button.set_label(view_mode_label(self.current_view));
                }
                self.current_date = date;
                self.refresh(widgets, &sender, root);
            }
            AppMsg::EventUpdated => {
                self.refresh(widgets, &sender, root);
            }
            AppMsg::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                widgets.sidebar_pane.set_visible(self.sidebar_visible);
            }
            AppMsg::CreateEvent => {
                let calendars = calendars_by_account(&self.core.storage).unwrap_or_default();
                let settings = load_settings(&self.core.storage).unwrap_or_default();
                match default_new_event(&calendars, &settings) {
                    Some(detail) => {
                        let ctx = EventCtx {
                            storage: self.core.storage.clone(),
                            sender: sender.clone(),
                            window: root.clone(),
                            date_format: resolve_date_format(&settings),
                            time_format: resolve_time_format(&settings),
                            docks: EditorDocks {
                                overlay: widgets.editor_overlay.clone(),
                                right_paned: widgets.right_dock_paned.clone(),
                                right_slot: widgets.right_dock_slot.clone(),
                                bottom_paned: widgets.bottom_dock_paned.clone(),
                                bottom_slot: widgets.bottom_dock_slot.clone(),
                                sidebar_pane: widgets.sidebar_pane.clone(),
                                main_content_box: widgets.main_content_box.clone(),
                                current_panel: self.open_editor_panel.clone(),
                            },
                        };
                        show_edit_event_dialog(detail, calendars, ctx);
                    }
                    None => tracing::warn!("no calendars available; cannot create a new event"),
                }
            }
            AppMsg::ShowPreferences => {
                let ctx = SettingsCtx {
                    storage: self.core.storage.clone(),
                    accounts: self.core.accounts.clone(),
                    sender: sender.clone(),
                    window: root.clone(),
                    world_clock_box: widgets.world_clock_box.clone(),
                };
                show_preferences_window(ctx);
            }
            AppMsg::SnoozeReminder { id, minutes } => {
                let until = notifications::snooze_until(minutes);
                if let Err(err) = snooze_reminder(&self.core.storage, id, until) {
                    tracing::warn!(%err, id, "failed to snooze reminder");
                }
                self.refresh_overlay(&sender, root);
            }
            AppMsg::DismissReminder(id) => {
                if let Err(err) = dismiss_reminder(&self.core.storage, id) {
                    tracing::warn!(%err, id, "failed to dismiss reminder");
                }
                self.refresh_overlay(&sender, root);
            }
            AppMsg::DismissAllReminders => {
                if let Err(err) = dismiss_all_active(&self.core.storage) {
                    tracing::warn!(%err, "failed to dismiss all reminders");
                }
                self.refresh_overlay(&sender, root);
            }
            AppMsg::SnoozeAllReminders(minutes) => {
                let until = notifications::snooze_until(minutes);
                if let Err(err) = snooze_all_active(&self.core.storage, until) {
                    tracing::warn!(%err, "failed to snooze all reminders");
                }
                self.refresh_overlay(&sender, root);
            }
            AppMsg::ClearHistoryEntry(id) => {
                if let Err(err) = delete_reminder_notification(&self.core.storage, id) {
                    tracing::warn!(%err, id, "failed to clear reminder history entry");
                }
                self.refresh_overlay(&sender, root);
            }
        }
    }

    fn update_cmd_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match message {
            AppCommandMsg::SyncFinished { synced, failed } => {
                tracing::info!(synced, failed, "sync now finished");
                self.refresh(widgets, &sender, root);
            }
            AppCommandMsg::CacheCleared { cleared, failed } => {
                tracing::info!(cleared, failed, "clear cache and resync finished");
                self.refresh(widgets, &sender, root);
            }
            AppCommandMsg::ReminderCheckFinished { newly_fired } => {
                if !newly_fired.is_empty() {
                    self.refresh_overlay(&sender, root);
                }
            }
        }
    }
}

impl App {
    /// Re-renders the month grid, mini calendar, header title, and sidebar calendar
    /// list from current model state — the shared tail end of every message that
    /// changes the displayed month, the active search filter, a calendar's visibility
    /// or color, or a saved event edit. The sidebar is rebuilt every time (not just
    /// after a color/"Display this only" change) so it stays one code path rather than
    /// special-casing which messages touch calendars other than the one clicked —
    /// e.g. "Display this only" flips visibility on every *other* row too, which a
    /// single row's own checkbox can't reflect on its own.
    /// Rebuilds the floating notification dialog from storage — the shared tail end
    /// of every `AppMsg`/`AppCommandMsg` that changes reminder state (snooze,
    /// dismiss, dismiss all, clear history, or a fresh scheduler tick).
    fn refresh_overlay(&self, sender: &ComponentSender<App>, root: &adw::Window) {
        let handle = notifications::ensure_overlay(&self.overlay, &self.core.storage, sender);
        notifications::refresh_overlay(&handle, root);
    }

    fn refresh(&self, widgets: &mut AppWidgets, sender: &ComponentSender<App>, root: &adw::Window) {
        let today = Local::now().date_naive();
        let settings = load_settings(&self.core.storage).unwrap_or_default();
        let events = events_for_visible_calendars(&self.core.storage).unwrap_or_default();
        let events = filter_events(&events, &self.search_query, settings.show_declined_events);
        let ctx = EventCtx {
            storage: self.core.storage.clone(),
            sender: sender.clone(),
            window: root.clone(),
            date_format: resolve_date_format(&settings),
            time_format: resolve_time_format(&settings),
            docks: EditorDocks {
                overlay: widgets.editor_overlay.clone(),
                right_paned: widgets.right_dock_paned.clone(),
                right_slot: widgets.right_dock_slot.clone(),
                bottom_paned: widgets.bottom_dock_paned.clone(),
                bottom_slot: widgets.bottom_dock_slot.clone(),
                sidebar_pane: widgets.sidebar_pane.clone(),
                main_content_box: widgets.main_content_box.clone(),
                current_panel: self.open_editor_panel.clone(),
            },
        };
        populate_month_grid(&widgets.month_grid, self.current_date, today, &events, &ctx);
        let day_view_dates: Vec<NaiveDate> = match self.current_view {
            ViewMode::FiveDay => five_day_window(self.current_date, settings.show_weekends),
            ViewMode::Custom => self.custom_view_dates.clone(),
            ViewMode::Day | ViewMode::Month => vec![self.current_date],
        };
        // Day view body first: it measures the hour grid's real gutter width, which
        // the header row and all-day strip then align their own gutter column to.
        let gutter_px = populate_day_view(
            &widgets.day_overlay,
            &day_view_dates,
            today,
            &events,
            &ctx,
            settings.day_time_scale_minutes,
            settings.day_drag_snap_ctrl_minutes,
            settings.day_drag_snap_ctrl_shift_minutes,
            settings.day_drag_hold_ms,
        );
        populate_day_header(
            &widgets.day_header_box,
            &widgets.day_all_day_box,
            &day_view_dates,
            today,
            &events,
            &ctx,
            gutter_px,
        );
        {
            let jump = jump_to_date_callback(sender);
            let range_pick = range_pick_callback(sender);
            populate_mini_calendar(
                &widgets.mini_calendar_grid,
                &widgets.mini_calendar_title,
                self.current_date,
                today,
                &jump,
                &jump,
                Some(&range_pick),
                if self.current_view == ViewMode::Custom { &self.custom_view_dates } else { &[] },
            );
        }
        let title = match self.current_view {
            ViewMode::Month => self.current_date.format("%B %Y").to_string(),
            ViewMode::Day => self.current_date.format("%A, %B %-d, %Y").to_string(),
            ViewMode::FiveDay => five_day_title(&day_view_dates),
            ViewMode::Custom => custom_view_title(&day_view_dates),
        };
        widgets.window_title.set_title(&title);

        let accounts = self.core.accounts.list_accounts().unwrap_or_default();
        let calendars = calendars_by_account(&self.core.storage).unwrap_or_default();
        load_calendar_color_css(&calendars);
        populate_sidebar(&widgets.sidebar_list, &accounts, &calendars, sender);
        populate_world_clock(&widgets.world_clock_box, &settings);
    }
}

/// Adds (or subtracts, for negative `delta`) whole months to `date`, always landing
/// on the 1st of the resulting month — `current_date` only ever needs to identify
/// *which month* is displayed, never a specific day within it.
fn shift_month(date: NaiveDate, delta: i32) -> NaiveDate {
    let total_months = date.year() * 12 + date.month() as i32 - 1 + delta;
    let year = total_months.div_euclid(12);
    let month = total_months.rem_euclid(12) as u32 + 1;
    NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month")
}

/// True for Saturday/Sunday — the only two days `AppSettings::show_weekends` affects
/// (DESIGN_SPEC.md §12), consulted by `five_day_window`/`step_weekdays`.
fn is_weekend(date: NaiveDate) -> bool {
    matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

/// Moves `date` by `delta` weekdays (skipping Sat/Sun), in either direction. Used by
/// `AppMsg::PrevPeriod`/`NextPeriod` to page `ViewMode::FiveDay`'s anchor when
/// `show_weekends` is off, so consecutive 5-day windows tile business days with no
/// overlap or gap.
fn step_weekdays(date: NaiveDate, delta: i32) -> NaiveDate {
    let step: i64 = if delta >= 0 { 1 } else { -1 };
    let mut d = date;
    let mut remaining = delta.unsigned_abs();
    while remaining > 0 {
        d += Duration::days(step);
        if !is_weekend(d) {
            remaining -= 1;
        }
    }
    d
}

/// The 5 dates shown in `ViewMode::FiveDay`, centered on `anchor` — 2 days before, the
/// anchor, 2 days after — matching real Google Calendar's "5 days" view (confirmed
/// against a reference screenshot: a Tue–Sat window with the Thursday "today" as the
/// 3rd of 5 columns), not the fixed Mon–Fri work week DESIGN_SPEC.md §12 describes.
/// Always returns exactly 5 dates. When `show_weekends` is false the window instead
/// spans 5 business days (skipping Sat/Sun entirely) — if `anchor` itself falls on a
/// weekend in that case, it's snapped forward to the following Monday first.
fn five_day_window(anchor: NaiveDate, show_weekends: bool) -> Vec<NaiveDate> {
    if show_weekends {
        return (-2..=2).map(|delta| anchor + Duration::days(delta)).collect();
    }

    let anchor = if is_weekend(anchor) { step_weekdays(anchor, 1) } else { anchor };

    let mut before = Vec::with_capacity(2);
    let mut d = anchor;
    while before.len() < 2 {
        d -= Duration::days(1);
        if !is_weekend(d) {
            before.push(d);
        }
    }
    before.reverse();

    let mut after = Vec::with_capacity(2);
    let mut d = anchor;
    while after.len() < 2 {
        d += Duration::days(1);
        if !is_weekend(d) {
            after.push(d);
        }
    }

    let mut dates = before;
    dates.push(anchor);
    dates.extend(after);
    dates
}

/// Window-title text for a date span: `"September 2026"` when both ends sit in one
/// month (matching Month view's own `"%B %Y"` convention at `App::refresh`), else a
/// spanning `"Aug 31 – Sep 4, 2026"` (or, across a year boundary,
/// `"Dec 29, 2025 – Jan 2, 2026"`). Shared by `five_day_title` and `custom_view_title`.
fn date_span_title(first: NaiveDate, last: NaiveDate) -> String {
    if first.year() == last.year() && first.month() == last.month() {
        first.format("%B %Y").to_string()
    } else if first.year() == last.year() {
        format!("{} – {}", first.format("%b %-d"), last.format("%b %-d, %Y"))
    } else {
        format!("{} – {}", first.format("%b %-d, %Y"), last.format("%b %-d, %Y"))
    }
}

/// Window-title text for `ViewMode::FiveDay`.
fn five_day_title(dates: &[NaiveDate]) -> String {
    date_span_title(dates[0], *dates.last().expect("five_day_window always returns 5 dates"))
}

/// Window-title text for `ViewMode::Custom` — same span-formatting as `five_day_title`,
/// just over however many dates the mini calendar drag-select produced.
fn custom_view_title(dates: &[NaiveDate]) -> String {
    date_span_title(dates[0], *dates.last().expect("custom_view_dates is never empty in ViewMode::Custom"))
}

/// Max consecutive dates a mini calendar drag-select can produce — the day/5-day view's
/// column layout is already generic over `&[NaiveDate]`, so this is purely a UX cap
/// (mirrors "5 day" view's own fixed width) rather than a rendering limit.
const MINI_CALENDAR_DRAG_MAX_DAYS: i64 = 8;

/// Consecutive dates from `origin` toward `other` (inclusive of both when the span
/// fits within `max_len`), capping the far end by trimming whichever side is farther
/// from `origin` — so the cell a mini calendar drag started on is always kept, and the
/// range only ever grows *toward* wherever the pointer currently is. `other == origin`
/// yields a single-date `vec![origin]`.
fn capped_date_range(origin: NaiveDate, other: NaiveDate, max_len: i64) -> Vec<NaiveDate> {
    let direction = if other >= origin { 1 } else { -1 };
    let raw_span = (other - origin).num_days().abs().min(max_len - 1);
    let far = origin + Duration::days(direction * raw_span);
    let (start, end) = if far >= origin { (origin, far) } else { (far, origin) };
    let mut dates = Vec::new();
    let mut d = start;
    while d <= end {
        dates.push(d);
        d += Duration::days(1);
    }
    dates
}

/// Case-insensitive substring match on title, mirroring DESIGN_SPEC.md §10's "simple
/// local full-text filter" — an empty query is treated as "no filter" rather than
/// matching nothing. Also applies §12's "Show declined events" preference: Google
/// Calendar's own semantics for that toggle are "on" = show declined events, dimmed
/// (`event_row`/`day_event_block` add an `.event-declined` CSS class for that half),
/// "off" = hide them from the grid entirely, which is the half this function owns —
/// done here, app-side, rather than in `events_for_visible_calendars`'s SQL, matching
/// where the search-query filter above already lives (a UI preference, not something
/// the storage layer needs to know about).
fn filter_events(events: &[DisplayEvent], query: &str, show_declined: bool) -> Vec<DisplayEvent> {
    let query = query.trim().to_lowercase();
    events
        .iter()
        .filter(|e| query.is_empty() || e.title.to_lowercase().contains(&query))
        .filter(|e| show_declined || e.self_response_status != Some(AttendeeResponseStatus::Declined))
        .cloned()
        .collect()
}

/// The blank starting point for the header bar's Create button (DESIGN_SPEC.md §10):
/// a slot starting at/after right now, on the Preferences window's "Default calendar"
/// (§12) — falling back to whichever calendar is first visible, then the first
/// calendar at all, if that setting is unset or points at a calendar that's since
/// been removed. `id: 0` marks it as not yet saved — see `show_edit_event_dialog`'s
/// doc comment. Returns `None` when there's nowhere to save a new event yet (no
/// calendars connected).
fn default_new_event(calendars: &[CalendarSummary], settings: &AppSettings) -> Option<EventDetail> {
    let calendar = settings
        .default_calendar_id
        .and_then(|id| calendars.iter().find(|c| c.id == id))
        .or_else(|| calendars.iter().find(|c| c.is_visible))
        .or_else(|| calendars.first())?;

    let duration = Duration::minutes(settings.default_event_duration_minutes.max(5));
    let round_to = settings.default_event_duration_minutes.clamp(5, 60);
    let now = Local::now();
    let round_up = Duration::minutes((round_to - (now.minute() as i64 % round_to)) % round_to);
    let start = (now + round_up).with_second(0).unwrap().with_nanosecond(0).unwrap();
    let end = start + duration;

    Some(EventDetail {
        id: 0,
        calendar_id: calendar.id,
        title: String::new(),
        description: None,
        location: None,
        start: start.to_rfc3339(),
        end: end.to_rfc3339(),
        all_day: false,
        color: None,
        reminders: vec![EventReminder {
            method: ReminderMethod::Popup,
            minutes: settings.default_reminder_minutes,
        }],
        busy: EventBusyStatus::Busy,
        visibility: EventVisibility::Default,
        recurrence: None,
        organizer_email: None,
        organizer_name: None,
        hangout_link: None,
        sequence: 0,
        created_at: None,
        self_response_status: None,
        attendees: Vec::new(),
        url: None,
        attachments: Vec::new(),
    })
}

/// Binds Ctrl+F (DESIGN_SPEC.md §12's search shortcut, alongside `/`) to focusing the
/// header bar's search field. Registered with `Global` scope on the window itself
/// rather than on the entry, so it fires no matter which widget currently has
/// keyboard focus — e.g. while the month grid or sidebar is focused.
fn install_search_shortcut(root: &impl IsA<gtk4::Widget>, search_entry: &gtk4::SearchEntry) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    let search_entry = search_entry.clone();
    controller.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("<Control>f"),
        Some(gtk4::CallbackAction::new(move |_widget, _args| {
            search_entry.grab_focus();
            gtk4::glib::Propagation::Stop
        })),
    ));

    root.add_controller(controller);
}

/// Binds Ctrl+, (DESIGN_SPEC.md §12: "a single libadwaita Preferences window (`Ctrl+,`,
/// or from the app menu)") to opening the Preferences window — `Global` scope for the
/// same reason as `install_search_shortcut`'s Ctrl+F.
fn install_preferences_shortcut(root: &impl IsA<gtk4::Widget>, sender: &ComponentSender<App>) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    let sender = sender.clone();
    controller.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("<Control>comma"),
        Some(gtk4::CallbackAction::new(move |_widget, _args| {
            sender.input(AppMsg::ShowPreferences);
            gtk4::glib::Propagation::Stop
        })),
    ));

    root.add_controller(controller);
}

/// Whether `widget`'s toplevel currently has an editable text widget focused — the
/// internal `Text` GtkEntry/SearchEntry/SpinButton/etc. delegate focus to, or a
/// `TextView` (the event editor's description field) — used by `install_view_shortcuts`
/// to tell "typing the letter d" apart from "pressing the D key" when both look like
/// the same keypress to a `Global`-scope shortcut.
fn focus_is_editable(widget: &gtk4::Widget) -> bool {
    widget.root().and_then(|root| root.focus()).is_some_and(|focus| focus.is::<gtk4::Text>() || focus.is::<gtk4::TextView>())
}

/// Binds Google Calendar's own single-letter view shortcuts (DESIGN_SPEC.md §10/§12's
/// "reused as-is" scheme) for the views that actually exist yet — `d` for Day, `m` for
/// Month, `x` for 5-day. `w`/`y`/`a` stay unbound, matching the shortcuts legend's
/// "documents the full intended scheme rather than only what's implemented so far"
/// note (see `SHORTCUT_GROUPS`), until Week/Year/Schedule views themselves land (§19).
///
/// `Global` scope, same as `install_search_shortcut`/`install_preferences_shortcut`, so
/// it fires while the month grid or day view has focus rather than only the window
/// chrome — but unlike Ctrl+F/Ctrl+,, a bare "d"/"m"/"x" is also ordinary text, so each
/// callback checks `focus_is_editable` first and lets the keystroke through as normal
/// input (`Propagation::Proceed`) whenever a text field currently has focus, rather
/// than hijacking every "d" typed into the event title or search box.
fn install_view_shortcuts(root: &impl IsA<gtk4::Widget>, sender: &ComponentSender<App>) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    for (key, view) in [("d", ViewMode::Day), ("m", ViewMode::Month), ("x", ViewMode::FiveDay)] {
        let sender = sender.clone();
        controller.add_shortcut(gtk4::Shortcut::new(
            gtk4::ShortcutTrigger::parse_string(key),
            Some(gtk4::CallbackAction::new(move |widget, _args| {
                if focus_is_editable(widget) {
                    return gtk4::glib::Propagation::Proceed;
                }
                sender.input(AppMsg::SetView(view));
                gtk4::glib::Propagation::Stop
            })),
        ));
    }

    root.add_controller(controller);
}

/// Restores the sidebar `Paned`'s dragged width from `fraction` (a saved
/// `sidebar_px / window_px` ratio) and wires persistence of future drags back to
/// storage. `fraction` is applied against `window`'s width once it reports a real
/// allocated size (same poll-until-nonzero idiom as `poll_for_real_width_then_resize`,
/// needed because `width()` still reads 0 at `init` time, before the compositor's
/// first layout pass) — this is what keeps the sidebar looking proportionally right
/// even if the fraction was saved on a different-sized or differently-scaled monitor.
///
/// `Paned` has no drag-end signal, only `notify::position`, which fires continuously
/// during a drag *and* for this function's own restoring `set_position()` call — so
/// `suppress_position_save` guards every programmatic move from being mistaken for a
/// user drag, and the actual save is debounced (cancel-and-reschedule) so a fast drag
/// doesn't hammer storage with a write per pixel.
fn install_sidebar_resize_persistence(paned: &gtk4::Paned, window: &adw::Window, storage: &Storage, fraction: f64) {
    let suppress_position_save: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let pending_save: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));

    {
        let storage = storage.clone();
        let window = window.clone();
        let suppress_position_save = suppress_position_save.clone();
        paned.connect_position_notify(move |paned| {
            if suppress_position_save.take() {
                return;
            }
            let raw = paned.position();
            let clamped = raw.clamp(SIDEBAR_MIN_WIDTH_PX, SIDEBAR_MAX_WIDTH_PX);
            if clamped != raw {
                paned.set_position(clamped); // re-enters here once with clamped == position(); no further recursion
                return;
            }
            if let Some(id) = pending_save.take() {
                id.remove();
            }
            let storage = storage.clone();
            let window = window.clone();
            let pending_save_for_timer = pending_save.clone();
            let id = gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
                pending_save_for_timer.set(None);
                let width = window.width().max(1);
                let mut settings = load_settings(&storage).unwrap_or_default();
                settings.sidebar_width_fraction = Some(clamped as f64 / width as f64);
                if let Err(err) = save_settings(&storage, &settings) {
                    tracing::warn!(%err, "failed to save sidebar width");
                }
            });
            pending_save.set(Some(id));
        });
    }

    let paned = paned.clone();
    let window = window.clone();
    let mut attempts_left = 40; // ~2s cap, same as poll_for_real_width_then_resize
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        attempts_left -= 1;
        if window.width() > 0 {
            let target = (window.width() as f64 * fraction).round() as i32;
            suppress_position_save.set(true);
            paned.set_position(target.clamp(SIDEBAR_MIN_WIDTH_PX, SIDEBAR_MAX_WIDTH_PX));
            return gtk4::glib::ControlFlow::Break;
        }
        if attempts_left <= 0 {
            return gtk4::glib::ControlFlow::Break;
        }
        gtk4::glib::ControlFlow::Continue
    });
}

/// Wires persistence of the right-docked event editor panel's drag-resized width —
/// same debounce/clamp shape as `install_sidebar_resize_persistence`, but attached
/// once at `init` time to the (always-present) `right_dock_paned`, independent of
/// whether the panel is currently docked into it, since that `Paned` lives for the
/// app's whole lifetime while the panel's own content only exists while open. No
/// startup-restore poll here, unlike the sidebar: this dock isn't on screen at
/// launch, so its saved fraction is only ever applied later, at the moment
/// `dock_panel` first docks the panel into it in a given session.
///
/// Because the dock is the `Paned`'s *end* child (the sidebar's fixed pane is its
/// *start* child), `position()` measures everything *before* the dock, not the
/// dock's own width — so the dock's width is `paned.width() - paned.position()`,
/// and clamping/persisting operate on that derived value, not `position()` itself.
/// `slot` (the dock's own `Box`) guards every `notify::position` firing: a hidden
/// `Paned` end child can still report shifting `position()` values as the window
/// resizes (all resize delta lands on the visible start side), which would
/// otherwise clamp-correct and persist a meaningless width while nothing is
/// actually docked here.
fn install_right_dock_resize_persistence(paned: &gtk4::Paned, slot: &gtk4::Box, storage: &Storage) {
    let pending_save: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));
    let slot = slot.clone();
    let storage = storage.clone();
    paned.connect_position_notify(move |paned| {
        if !slot.is_visible() {
            return;
        }
        let total = paned.width().max(1);
        let raw_dock_width = total - paned.position();
        let clamped = raw_dock_width.clamp(EDITOR_RIGHT_DOCK_MIN_WIDTH_PX, EDITOR_RIGHT_DOCK_MAX_WIDTH_PX);
        if clamped != raw_dock_width {
            paned.set_position(total - clamped); // re-enters here once with clamped == derived width; no further recursion
            return;
        }
        if let Some(id) = pending_save.take() {
            id.remove();
        }
        let storage = storage.clone();
        let pending_save_for_timer = pending_save.clone();
        let id = gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
            pending_save_for_timer.set(None);
            let mut settings = load_settings(&storage).unwrap_or_default();
            settings.event_editor_right_width_fraction = Some(clamped as f64 / total as f64);
            if let Err(err) = save_settings(&storage, &settings) {
                tracing::warn!(%err, "failed to save event editor right dock width");
            }
        });
        pending_save.set(Some(id));
    });
}

/// Bottom-dock counterpart of `install_right_dock_resize_persistence` — same
/// derived-size-from-the-end-child arithmetic and hidden-slot guard, just measuring
/// height off a vertical `Paned` instead of width off a horizontal one.
fn install_bottom_dock_resize_persistence(paned: &gtk4::Paned, slot: &gtk4::Box, storage: &Storage) {
    let pending_save: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));
    let slot = slot.clone();
    let storage = storage.clone();
    paned.connect_position_notify(move |paned| {
        if !slot.is_visible() {
            return;
        }
        let total = paned.height().max(1);
        let raw_dock_height = total - paned.position();
        let clamped = raw_dock_height.clamp(EDITOR_BOTTOM_DOCK_MIN_HEIGHT_PX, EDITOR_BOTTOM_DOCK_MAX_HEIGHT_PX);
        if clamped != raw_dock_height {
            paned.set_position(total - clamped);
            return;
        }
        if let Some(id) = pending_save.take() {
            id.remove();
        }
        let storage = storage.clone();
        let pending_save_for_timer = pending_save.clone();
        let id = gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
            pending_save_for_timer.set(None);
            let mut settings = load_settings(&storage).unwrap_or_default();
            settings.event_editor_bottom_height_fraction = Some(clamped as f64 / total as f64);
            if let Err(err) = save_settings(&storage, &settings) {
                tracing::warn!(%err, "failed to save event editor bottom dock height");
            }
        });
        pending_save.set(Some(id));
    });
}

/// Zeroes out (or restores) the shared-edge margin on the widgets adjacent to
/// whichever dock is active, so a docked event editor panel gets a clean single
/// handle-width gap against its neighbor instead of a doubled gap from two
/// independent outer margins meeting at the same edge — see `EditorDocks`'s doc
/// comment and DESIGN_SPEC.md §10's "Motion & feel" for why this app's panes only
/// ever carry a margin on edges that don't already border a `Paned` handle.
/// `main_content_box`'s margin is affected by *either* dock — its right edge
/// borders `right_dock_paned`'s handle when right-docked, its bottom edge borders
/// `bottom_dock_paned`'s handle when bottom-docked. The sidebar sits outside both
/// docks' `Paned`s entirely (`sidebar_paned` wraps everything else, full window
/// height) so it never needs a margin adjustment here — bottom-docking only ever
/// cuts into the main content column.
fn update_dock_margins(docks: &EditorDocks, mode: EventEditorPanelMode) {
    let right_active = mode == EventEditorPanelMode::Right;
    let bottom_active = mode == EventEditorPanelMode::Bottom;
    docks.main_content_box.set_margin_end(if right_active { 0 } else { 12 });
    docks.main_content_box.set_margin_bottom(if bottom_active { 0 } else { 12 });
}

/// Detaches the event editor panel's root from wherever it currently lives (the
/// floating overlay, or one of the two dock slots — a no-op if it has no parent yet)
/// and restores both dock slots/margins to their empty, non-docked state. This is
/// the tear-down half of the panel's lifecycle — called for Save, Duplicate, Delete,
/// and a confirmed Discard, i.e. every path that actually ends the panel's life,
/// never for a live floating<->docked drag transition (`dock_panel`/`attach_floating`/
/// `attach_docked_right`/`attach_docked_bottom` reparent the same still-open panel
/// instead of tearing it down).
fn close_panel(panel_root: &adw::ToolbarView, docks: &EditorDocks) {
    panel_root.unparent();
    docks.right_slot.set_visible(false);
    docks.bottom_slot.set_visible(false);
    update_dock_margins(docks, EventEditorPanelMode::Floating);
    // Tell the persistent overlay-level drag gesture there's no panel to drag
    // anymore — otherwise a press on whatever now overlaps the old titlebar's
    // former bounds could spuriously hit-test against a panel that's already gone.
    *docks.current_panel.borrow_mut() = None;
}

/// Styles the panel root for a docked (Bottom or Right) presentation: fills its
/// slot instead of sizing to its own natural/floating size, and drops the floating
/// look (see `attach_floating`'s `.floating-editor-panel` class).
fn style_panel_as_docked(panel_root: &adw::ToolbarView) {
    panel_root.remove_css_class("floating-editor-panel");
    // Defensive — present if this dock was just committed straight out of a live
    // preview (`preview_docked_right`/`preview_docked_bottom`); a real dock slot's
    // own ancestry already gives the panel an opaque background, so the preview-only
    // class would be redundant here, not wrong, but there's no reason to leave it on.
    panel_root.remove_css_class("docking-editor-panel");
    panel_root.set_size_request(-1, -1);
    panel_root.set_halign(gtk4::Align::Fill);
    panel_root.set_valign(gtk4::Align::Fill);
    panel_root.set_hexpand(true);
    panel_root.set_vexpand(true);
    panel_root.set_margin_start(0);
    panel_root.set_margin_top(0);
}

/// Reparents the panel into the overlay as a freely-positioned floating card at
/// `(margin_x, margin_y)` — the "look like `notifications.rs`'s own floating dialog"
/// styling (border + rounded corners + `@window_bg_color`, via the
/// `.floating-editor-panel` CSS class in `load_static_css`) plus a fixed size (see
/// `DEFAULT_EDITOR_FLOATING_SIZE_PX`'s doc comment for why floating isn't
/// independently resizable). Used both for the initial "open floating" attach
/// (`dock_panel`) and for a live undock mid-drag (`install_panel_drag`), which is why
/// the position is a plain parameter rather than read from settings here — the drag
/// case wants the panel to appear wherever the pointer just pulled it to, not
/// wherever it was last saved.
fn attach_floating(panel_root: &adw::ToolbarView, docks: &EditorDocks, margin_x: i32, margin_y: i32) {
    panel_root.unparent();
    docks.right_slot.set_visible(false);
    docks.bottom_slot.set_visible(false);
    update_dock_margins(docks, EventEditorPanelMode::Floating);

    panel_root.remove_css_class("docking-editor-panel");
    panel_root.add_css_class("floating-editor-panel");
    // `Overflow::Hidden` alongside the CSS class, same as `.card`'s own convention —
    // otherwise the header bar's square corners would poke past the rounded border.
    panel_root.set_overflow(gtk4::Overflow::Hidden);
    let (width, height) = DEFAULT_EDITOR_FLOATING_SIZE_PX;
    panel_root.set_size_request(width, height);
    panel_root.set_halign(gtk4::Align::Start);
    panel_root.set_valign(gtk4::Align::Start);
    panel_root.set_hexpand(false);
    panel_root.set_vexpand(false);
    panel_root.set_margin_start(margin_x.max(0));
    panel_root.set_margin_top(margin_y.max(0));
    docks.overlay.add_overlay(panel_root);
}

/// Reparents the panel into `right_dock_slot`, restoring its remembered dock width
/// (`AppSettings::event_editor_right_width_fraction`, falling back to the built-in
/// default) against `right_dock_paned`'s *current* width — read synchronously rather
/// than polled, since (unlike the sidebar, visible from launch) this dock is never
/// touched before the main window already has a real allocated size (see
/// `install_right_dock_resize_persistence`'s doc comment).
fn attach_docked_right(panel_root: &adw::ToolbarView, docks: &EditorDocks, storage: &Storage) {
    panel_root.unparent();
    docks.bottom_slot.set_visible(false);
    update_dock_margins(docks, EventEditorPanelMode::Right);
    style_panel_as_docked(panel_root);

    docks.right_slot.append(panel_root);
    docks.right_slot.set_visible(true);

    let settings = load_settings(storage).unwrap_or_default();
    let total = docks.right_paned.width().max(1);
    let fraction = settings
        .event_editor_right_width_fraction
        .unwrap_or(DEFAULT_EDITOR_RIGHT_DOCK_WIDTH_PX as f64 / 1100.0);
    let target_dock_width = (total as f64 * fraction).round() as i32;
    docks.right_paned.set_position((total - target_dock_width).clamp(0, total));
}

/// Bottom-dock counterpart of `attach_docked_right`.
fn attach_docked_bottom(panel_root: &adw::ToolbarView, docks: &EditorDocks, storage: &Storage) {
    panel_root.unparent();
    docks.right_slot.set_visible(false);
    update_dock_margins(docks, EventEditorPanelMode::Bottom);
    style_panel_as_docked(panel_root);

    docks.bottom_slot.append(panel_root);
    docks.bottom_slot.set_visible(true);

    let settings = load_settings(storage).unwrap_or_default();
    let total = docks.bottom_paned.height().max(1);
    let fraction = settings
        .event_editor_bottom_height_fraction
        .unwrap_or(DEFAULT_EDITOR_BOTTOM_DOCK_HEIGHT_PX as f64 / 720.0);
    let target_dock_height = (total as f64 * fraction).round() as i32;
    docks.bottom_paned.set_position((total - target_dock_height).clamp(0, total));
}

/// The event editor panel's field rows, bundled so `relayout_editor_body` can freely
/// reparent them between a single-column stack and a wide multi-column arrangement.
/// Each field is already exactly one self-contained `gtk4::Box` (`field_row`'s own
/// icon+widget row, or a row like `date_time_row`/`options_row` built directly as a
/// `Box`), so moving a whole row between containers never disturbs the individual
/// field widgets' own signal handlers/state nested inside it.
struct EditorBodyRows {
    date_time_row: gtk4::Box,
    options_row: gtk4::Box,
    location_row: gtk4::Box,
    calendar_row: gtk4::Box,
    notifications_row: gtk4::Box,
    busy_visibility_row: gtk4::Box,
    description_row: gtk4::Box,
}

/// Detaches `widget` from its current parent (if any) and appends it to `container`
/// — the reparent primitive `relayout_editor_body` uses to move a field row between
/// layouts. Guarded rather than an unconditional `unparent()` because a row that's
/// already landed in its target arrangement (e.g. the very first layout call, before
/// any row has ever had a parent) has nothing to detach from.
fn move_into(container: &gtk4::Box, widget: &impl IsA<gtk4::Widget>) {
    if widget.parent().is_some() {
        widget.unparent();
    }
    container.append(widget);
}

/// Re-flows the event editor panel's field rows for `mode` — called once for the
/// panel's starting mode and again every time a live drag commits to a new mode
/// (`OpenEditorPanel::relayout`). The single-column stack (Floating/Right) matches
/// those two modes' narrow-but-tall shape; Bottom is wide but short, so its fields
/// split into two side-by-side columns instead, roughly halving the stack's total
/// height while actually using the dock's extra width — with Description (the field
/// most likely to want room to read/edit) spanning the full width below both
/// columns rather than being squeezed into either one.
fn relayout_editor_body(body: &gtk4::Box, rows: &EditorBodyRows, mode: EventEditorPanelMode) {
    clear_children(body);
    match mode {
        EventEditorPanelMode::Bottom => {
            let columns = gtk4::Box::new(gtk4::Orientation::Horizontal, 24);
            let left = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
            left.set_hexpand(true);
            for row in [&rows.date_time_row, &rows.options_row, &rows.location_row, &rows.calendar_row] {
                move_into(&left, row);
            }
            let right = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
            right.set_hexpand(true);
            for row in [&rows.notifications_row, &rows.busy_visibility_row] {
                move_into(&right, row);
            }
            columns.append(&left);
            columns.append(&right);
            body.append(&columns);
            move_into(body, &rows.description_row);
        }
        EventEditorPanelMode::Floating | EventEditorPanelMode::Right => {
            for row in [
                &rows.date_time_row,
                &rows.options_row,
                &rows.location_row,
                &rows.calendar_row,
                &rows.notifications_row,
                &rows.busy_visibility_row,
                &rows.description_row,
            ] {
                move_into(body, row);
            }
        }
    }
}

/// Attaches the panel into whichever container `mode` calls for, restoring that
/// mode's remembered size/position from settings. Used once, when the panel first
/// opens, to render it in the persisted starting mode — mid-drag transitions call
/// `attach_floating`/`attach_docked_right`/`attach_docked_bottom` directly instead
/// (see `attach_floating`'s doc comment for why floating specifically needs that).
fn dock_panel(panel_root: &adw::ToolbarView, mode: EventEditorPanelMode, docks: &EditorDocks, storage: &Storage) {
    match mode {
        EventEditorPanelMode::Floating => {
            let settings = load_settings(storage).unwrap_or_default();
            let overlay_w = docks.overlay.width().max(1) as f64;
            let overlay_h = docks.overlay.height().max(1) as f64;
            let (margin_x, margin_y) = settings
                .event_editor_floating_position
                .map(|(fx, fy)| ((fx * overlay_w).round() as i32, (fy * overlay_h).round() as i32))
                .unwrap_or(DEFAULT_EDITOR_FLOATING_MARGIN_PX);
            attach_floating(panel_root, docks, margin_x, margin_y);
        }
        EventEditorPanelMode::Right => attach_docked_right(panel_root, docks, storage),
        EventEditorPanelMode::Bottom => attach_docked_bottom(panel_root, docks, storage),
    }
}

/// Panel equivalent of `install_close_guard`: runs `on_close` immediately if nothing
/// has changed, otherwise shows the same "Discard unsaved changes?" `AlertDialog`.
/// Called directly from both trigger points (the header's close button and Escape)
/// instead of being installed as a `close-request` signal handler, since the panel
/// is a plain embedded widget with no such signal to hang this off of. `window` is
/// the *main* app window (`ctx.window`), used only as the confirmation dialog's
/// transient parent — there's no separate top-level window of the panel's own left.
fn confirm_close(window: &adw::Window, dirty: &Rc<Cell<bool>>, on_close: impl Fn() + 'static) {
    if !dirty.get() {
        on_close();
        return;
    }

    let confirm = adw::AlertDialog::new(
        Some("Discard unsaved changes?"),
        Some("This event has changes that haven't been saved."),
    );
    confirm.add_response("keep-editing", "Keep Editing");
    confirm.add_response("discard", "Discard");
    confirm.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("keep-editing"));
    confirm.set_close_response("keep-editing");

    confirm.choose(Some(window), gtk4::gio::Cancellable::NONE, move |response| {
        if response == "discard" {
            on_close();
        }
    });
}

/// Panel equivalent of `install_escape_to_close`: binds Escape to `on_close` via the
/// same `Global`-scope `gtk4::ShortcutController` idiom, attached to the panel's own
/// root instead of a window. `Global` scope resolves at the nearest `Root` ancestor
/// regardless of which specific widget currently has keyboard focus — the title
/// entry, the description text view, etc. — the same as the window-based version,
/// since a `Root` ancestor exists as soon as this panel is parented anywhere in the
/// main window's tree (`install_view_shortcuts`/`install_search_shortcut` already
/// establish that a `Global`-scope controller need not be attached to a toplevel
/// itself, only live under one).
fn install_panel_escape_to_close(panel_root: &adw::ToolbarView, on_close: impl Fn() + 'static) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    controller.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("Escape"),
        Some(gtk4::CallbackAction::new(move |_widget, _args| {
            on_close();
            gtk4::glib::Propagation::Stop
        })),
    ));

    panel_root.add_controller(controller);
}

/// Live "would look floating here" preview during a drag — the same visual result
/// `attach_floating` gives (size, CSS class, margins) without any of its
/// unparent/reparent side effects. Called on every `drag-update` tick while the
/// dragged panel started the gesture already floating (see
/// `install_panel_drag`'s doc comment for why a *docked* start can't get this same
/// live preview).
fn preview_floating(panel_root: &adw::ToolbarView, margin_x: i32, margin_y: i32) {
    panel_root.remove_css_class("docking-editor-panel");
    panel_root.add_css_class("floating-editor-panel");
    let (width, height) = DEFAULT_EDITOR_FLOATING_SIZE_PX;
    panel_root.set_halign(gtk4::Align::Start);
    panel_root.set_valign(gtk4::Align::Start);
    panel_root.set_hexpand(false);
    panel_root.set_vexpand(false);
    panel_root.set_size_request(width, height);
    panel_root.set_margin_start(margin_x.max(0));
    panel_root.set_margin_top(margin_y.max(0));
}

/// Live "would snap into the right dock here" preview — the same shape
/// `attach_docked_right` would give the panel once actually docked, applied
/// purely cosmetically: still an `editor_overlay` child, not yet reparented into
/// `right_dock_slot` (see `install_panel_drag`'s doc comment for why that part
/// has to wait for `drag-end`). Swaps `.floating-editor-panel` for
/// `.docking-editor-panel` rather than just dropping it, so the panel stays fully
/// opaque throughout instead of going translucent mid-preview (see that class's
/// doc comment in `load_static_css`).
fn preview_docked_right(panel_root: &adw::ToolbarView, docks: &EditorDocks, storage: &Storage) {
    panel_root.remove_css_class("floating-editor-panel");
    panel_root.add_css_class("docking-editor-panel");
    let overlay_w = docks.overlay.width().max(1);
    let overlay_h = docks.overlay.height().max(1);
    let settings = load_settings(storage).unwrap_or_default();
    let fraction = settings
        .event_editor_right_width_fraction
        .unwrap_or(DEFAULT_EDITOR_RIGHT_DOCK_WIDTH_PX as f64 / 1100.0);
    let width = ((overlay_w as f64) * fraction).round() as i32;
    panel_root.set_halign(gtk4::Align::Start);
    panel_root.set_valign(gtk4::Align::Start);
    panel_root.set_hexpand(false);
    panel_root.set_vexpand(false);
    panel_root.set_size_request(width, overlay_h);
    panel_root.set_margin_top(0);
    panel_root.set_margin_start((overlay_w - width).max(0));
}

/// Bottom-dock counterpart of `preview_docked_right`.
fn preview_docked_bottom(panel_root: &adw::ToolbarView, docks: &EditorDocks, storage: &Storage) {
    panel_root.remove_css_class("floating-editor-panel");
    panel_root.add_css_class("docking-editor-panel");
    let overlay_w = docks.overlay.width().max(1);
    let overlay_h = docks.overlay.height().max(1);
    // The real bottom dock only spans the main content column (`sidebar_paned`
    // wraps it, full window height, entirely outside the bottom dock's own
    // `Paned`) — offsetting the preview by the sidebar's current width keeps it
    // from covering the sidebar, matching what actually happens once really
    // docked instead of jumping narrower at drag-end.
    let sidebar_w = docks.sidebar_pane.width().max(0);
    let content_w = (overlay_w - sidebar_w).max(1);
    let settings = load_settings(storage).unwrap_or_default();
    let fraction = settings
        .event_editor_bottom_height_fraction
        .unwrap_or(DEFAULT_EDITOR_BOTTOM_DOCK_HEIGHT_PX as f64 / 720.0);
    let height = ((overlay_h as f64) * fraction).round() as i32;
    panel_root.set_halign(gtk4::Align::Start);
    panel_root.set_valign(gtk4::Align::Start);
    panel_root.set_hexpand(false);
    panel_root.set_vexpand(false);
    panel_root.set_size_request(content_w, height);
    panel_root.set_margin_start(sidebar_w);
    panel_root.set_margin_top((overlay_h - height).max(0));
}

/// Wires the *one* persistent drag gesture that repositions/(un)docks the event
/// editor panel — installed once, here, on `editor_overlay` at `init` time, never
/// on the panel or its titlebar directly, and never reinstalled per panel-open.
///
/// This indirection is required, not just convenient. GTK4 resets/cancels a
/// widget's in-progress gesture sequence whenever that widget is unmapped, and
/// reparenting a widget (`unparent` + re-add elsewhere) always unmaps it, however
/// briefly. Docking/undocking the panel necessarily reparents it (between
/// `editor_overlay` and a dock `Paned`'s end-child slot) — so a `GestureDrag`
/// attached directly to the panel's own titlebar would die the instant that
/// reparent happened mid-drag, leaving the panel unresponsive to the rest of that
/// same physical drag: it snaps into (or out of) a dock once, and then stops
/// responding even though the pointer is still held down. `editor_overlay` itself
/// is a permanent part of the main window and is never reparented, so a gesture
/// attached there survives every transition; `docks.current_panel` is how it
/// finds out, dynamically, which panel (if any) is currently open and where its
/// titlebar currently is, since the panel itself is rebuilt fresh on every open
/// rather than being a persistent singleton.
///
/// To keep the gesture alive for the panel's *entire* physical drag, the actual
/// reparent between the floating overlay and a real dock `Paned` slot is deferred
/// to `drag-end` (safe — the gesture is already concluding there, so nothing more
/// needs to reach it afterward) rather than happening live mid-drag. Reparenting
/// from inside a `drag-update` handler was tried and reverted — even though the
/// gesture itself lives on `editor_overlay` rather than the panel, unparenting the
/// panel while a pointer-motion event is still being dispatched through it left
/// the whole window's input unresponsive, so that reparent genuinely has to wait
/// for `drag-end`. A drag that starts *floating* still gets a live preview as it
/// nears an edge (`preview_docked_right`/`preview_docked_bottom` resize/reposition
/// the same overlay-hosted widget to look like the dock it's about to become,
/// without reparenting); a drag that starts *docked* can't get that same live
/// preview without reparenting early — a docked panel is a `Paned` end-child, whose
/// geometry that `Paned` fully owns — so it shows no visual change until release,
/// at which point it either snaps back into the same dock (no visible change at
/// all) or pops out to floating at the final drag position.
fn install_panel_drag(overlay: &gtk4::Overlay, docks: &EditorDocks, storage: &Storage) {
    let drag = gtk4::GestureDrag::new();
    // Populated at `drag-begin` from `docks.current_panel` and read for the rest
    // of that one gesture — re-reading `current_panel` on every tick would be
    // wrong anyway, since a panel could close mid-drag (e.g. via Escape) and this
    // gesture shouldn't suddenly start acting on whatever opens next.
    let active_panel: Rc<RefCell<Option<OpenEditorPanel>>> = Rc::new(RefCell::new(None));
    let last_offset: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
    // The panel's logical top-left if it were floating right now — tracked
    // independently of `panel_root`'s actual on-screen margins, since those get
    // overwritten with a *dock-shaped* preview whenever `pending_mode` isn't
    // `Floating`; this is what lets the floating position keep tracking the
    // pointer correctly even while the panel is currently rendering as a dock
    // preview.
    let drag_pos: Rc<Cell<(i32, i32)>> = Rc::new(Cell::new((0, 0)));
    let pending_mode: Rc<Cell<EventEditorPanelMode>> = Rc::new(Cell::new(EventEditorPanelMode::Floating));
    // What layout `relayout_editor_body` last arranged the panel's *content* into —
    // distinct from `panel.mode` (which tracks whether the panel has *really* been
    // reparented out of a dock yet, flipping at most once per drag). This instead
    // tracks the live preview's current shape so the field layout stays in lock
    // step with whatever size/shape `preview_floating`/`preview_docked_right`/
    // `preview_docked_bottom` is showing at each tick — otherwise the outer panel
    // resizes to a dock's shape while its content is still arranged for whatever
    // shape it *was*, until release finally reconciles them.
    let applied_layout: Rc<Cell<EventEditorPanelMode>> = Rc::new(Cell::new(EventEditorPanelMode::Floating));

    {
        let docks = docks.clone();
        let active_panel = active_panel.clone();
        let last_offset = last_offset.clone();
        let drag_pos = drag_pos.clone();
        let pending_mode = pending_mode.clone();
        let applied_layout = applied_layout.clone();
        drag.connect_drag_begin(move |gesture, start_x, start_y| {
            let panel = docks.current_panel.borrow().clone();
            let press_point = gtk4::graphene::Point::new(start_x as f32, start_y as f32);
            let hit = panel.as_ref().is_some_and(|panel| {
                panel
                    .titlebar
                    .compute_bounds(&docks.overlay)
                    .is_some_and(|bounds| bounds.contains_point(&press_point))
            });
            if !hit {
                // Not a press on the titlebar (or no panel open at all) — leave it
                // for whoever else wants it rather than claiming a sequence this
                // gesture has nothing to do with.
                gesture.set_state(gtk4::EventSequenceState::Denied);
                *active_panel.borrow_mut() = None;
                return;
            }
            // Claimed, not left unclaimed for whatever provides this window's own
            // CSD interactive-move handling to also see — see
            // `install_day_event_drag` for the same reasoning/precedent; an
            // unclaimed sequence here is exactly what used to let dragging the
            // panel move the whole main window instead.
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let panel = panel.expect("`hit` is only true when `panel` is `Some`");
            let current_mode = panel.mode.get();
            pending_mode.set(current_mode);
            applied_layout.set(current_mode);
            last_offset.set((0.0, 0.0));

            let start_pos = if current_mode == EventEditorPanelMode::Floating {
                (panel.root.margin_start(), panel.root.margin_top())
            } else {
                // Starting docked: nothing to read a floating position from yet —
                // seed it from the titlebar's current on-screen spot instead, so
                // *if* this drag ends up resolving to Floating, it starts from
                // wherever the panel visually was rather than jumping.
                panel
                    .titlebar
                    .compute_point(&docks.overlay, &gtk4::graphene::Point::new(0.0, 0.0))
                    .map(|point| (point.x().max(0.0).round() as i32, point.y().max(0.0).round() as i32))
                    .unwrap_or(DEFAULT_EDITOR_FLOATING_MARGIN_PX)
            };
            drag_pos.set(start_pos);
            *active_panel.borrow_mut() = Some(panel);
        });
    }
    {
        let docks = docks.clone();
        let storage = storage.clone();
        let active_panel = active_panel.clone();
        let last_offset = last_offset.clone();
        let drag_pos = drag_pos.clone();
        let pending_mode = pending_mode.clone();
        let applied_layout = applied_layout.clone();
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            let Some(panel) = active_panel.borrow().clone() else { return };

            // A docked-start panel can't preview live in place (see this
            // function's doc comment) — pop it out to floating for real instead.
            // Deferred to the main loop's next idle pass rather than done
            // synchronously here: reparenting directly from inside a
            // `drag-update` handler, while a pointer-motion event is still being
            // dispatched through the panel, previously left the whole window's
            // input unresponsive. Marking `panel.mode` as `Floating` right away
            // (before the reparent has actually happened) keeps this branch from
            // firing again on the tick or two before the idle callback runs.
            if panel.mode.get() != EventEditorPanelMode::Floating {
                panel.mode.set(EventEditorPanelMode::Floating);
                applied_layout.set(EventEditorPanelMode::Floating);
                let panel_root = panel.root.clone();
                let docks = docks.clone();
                let active_panel = active_panel.clone();
                let relayout = panel.relayout.clone();
                let (start_x, start_y) = drag_pos.get();
                gtk4::glib::idle_add_local_once(move || {
                    // Skip if `drag-end` already concluded this gesture (and
                    // reparented the panel itself, possibly into a dock rather
                    // than floating) before this had a chance to run —
                    // reparenting it again here would undo that.
                    if active_panel.borrow().is_some() {
                        attach_floating(&panel_root, &docks, start_x, start_y);
                        // Without this, the panel's field rows are still in
                        // whatever arrangement the dock it just left used (e.g.
                        // Bottom's wide two-column layout) — that content's own
                        // natural width overrides the 560px floating size request
                        // (a size request is only a floor, not a cap), so the
                        // panel renders far wider than a floating panel should
                        // until this catches it up to match.
                        relayout(EventEditorPanelMode::Floating);
                    }
                });
                return;
            }

            let (last_x, last_y) = last_offset.get();
            let (dx, dy) = (offset_x - last_x, offset_y - last_y);
            last_offset.set((offset_x, offset_y));

            let overlay_w = docks.overlay.width().max(1);
            let overlay_h = docks.overlay.height().max(1);
            let (floating_w, floating_h) = DEFAULT_EDITOR_FLOATING_SIZE_PX;

            let (prev_x, prev_y) = drag_pos.get();
            let clamped_x = (prev_x as f64 + dx).round() as i32;
            let clamped_y = (prev_y as f64 + dy).round() as i32;
            let clamped_x = clamped_x.clamp(0, (overlay_w - floating_w).max(0));
            let clamped_y = clamped_y.clamp(0, (overlay_h - floating_h).max(0));
            drag_pos.set((clamped_x, clamped_y));

            let distance_to_right = (overlay_w - (clamped_x + floating_w)).max(0) as f64;
            let distance_to_bottom = (overlay_h - (clamped_y + floating_h)).max(0) as f64;
            let new_pending = if distance_to_bottom < EDITOR_PANEL_SNAP_THRESHOLD_PX && distance_to_bottom <= distance_to_right
            {
                EventEditorPanelMode::Bottom
            } else if distance_to_right < EDITOR_PANEL_SNAP_THRESHOLD_PX {
                EventEditorPanelMode::Right
            } else {
                EventEditorPanelMode::Floating
            };
            pending_mode.set(new_pending);

            // Keep the panel's *content* arrangement in step with whatever shape
            // the live preview is about to show — otherwise e.g. floating-drag-
            // toward-the-bottom-edge would resize the outer panel to the wide
            // dock preview while its fields are still in the narrow single-column
            // layout (or vice versa), a size/content mismatch that would only
            // reconcile at release. Only re-relayouts when the shape actually
            // changes, not on every tick.
            if new_pending != applied_layout.get() {
                (panel.relayout)(new_pending);
                applied_layout.set(new_pending);
            }

            // By this point `panel.mode` always reads `Floating` — either this
            // drag started that way, or the pop-out above just made it so (the
            // early `return` there skips this same tick, but every tick from here
            // on reaches this) — so every drag gets the same live preview.
            match new_pending {
                EventEditorPanelMode::Floating => preview_floating(&panel.root, clamped_x, clamped_y),
                EventEditorPanelMode::Right => preview_docked_right(&panel.root, &docks, &storage),
                EventEditorPanelMode::Bottom => preview_docked_bottom(&panel.root, &docks, &storage),
            }
        });
    }
    {
        let docks = docks.clone();
        let storage = storage.clone();
        let active_panel = active_panel.clone();
        let drag_pos = drag_pos.clone();
        let pending_mode = pending_mode.clone();
        drag.connect_drag_end(move |_, _, _| {
            let Some(panel) = active_panel.borrow_mut().take() else { return };
            let resolved = pending_mode.get();
            panel.mode.set(resolved);
            let (x, y) = drag_pos.get();
            // The one and only reparent for this whole gesture — safe here
            // specifically because the gesture is already concluding, so there's
            // nothing left that could be disrupted by the unmap/remap it causes.
            // Applied unconditionally, even if `resolved` matches where the panel
            // started (e.g. a docked panel nudged only slightly): idempotent, and
            // simpler than special-casing "didn't actually change".
            match resolved {
                EventEditorPanelMode::Floating => attach_floating(&panel.root, &docks, x, y),
                EventEditorPanelMode::Right => attach_docked_right(&panel.root, &docks, &storage),
                EventEditorPanelMode::Bottom => attach_docked_bottom(&panel.root, &docks, &storage),
            }
            (panel.relayout)(resolved);

            let mut settings = load_settings(&storage).unwrap_or_default();
            settings.event_editor_panel_mode = resolved;
            if resolved == EventEditorPanelMode::Floating {
                let overlay_w = docks.overlay.width().max(1) as f64;
                let overlay_h = docks.overlay.height().max(1) as f64;
                settings.event_editor_floating_position = Some((x as f64 / overlay_w, y as f64 / overlay_h));
            }
            if let Err(err) = save_settings(&storage, &settings) {
                tracing::warn!(%err, "failed to save event editor panel mode/position");
            }
        });
    }

    overlay.add_controller(drag);
}

/// Binds Ctrl+scroll on the Day view's hour grid to `AppMsg::ZoomDayTimeScale`
/// (DESIGN_SPEC.md §12's Time scale option) — scrolling up steps to a finer interval
/// (zoom in), scrolling down to a coarser one (zoom out). Installed with
/// `PropagationPhase::Capture` so it sees the scroll event before the `ScrolledWindow`'s
/// own internal scroll controller does: when Ctrl isn't held, this returns
/// `Propagation::Proceed` and the event falls through to normal panning untouched; when
/// it is, this consumes the event (`Propagation::Stop`) so Ctrl+scroll zooms instead of
/// simultaneously scrolling the view.
fn install_day_zoom_controller(scroller: &gtk4::ScrolledWindow, sender: &ComponentSender<App>) {
    let controller = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

    let sender = sender.clone();
    controller.connect_scroll(move |controller, _dx, dy| {
        if !controller.current_event_state().contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
            return gtk4::glib::Propagation::Proceed;
        }
        sender.input(AppMsg::ZoomDayTimeScale { finer: dy < 0.0 });
        gtk4::glib::Propagation::Stop
    });

    scroller.add_controller(controller);
}

/// Polls every 50ms until `widget` reports a real allocated width *and* height, then
/// fires `AppMsg::Resize` once — `App::refresh` repopulates both the month grid and the
/// Day view's event columns unconditionally, so one message corrects whichever of them
/// actually needed the real size. Two distinct reasons this is needed, both handled by
/// the same self-expiring poll (gives up after ~2s rather than running forever if a
/// widget never becomes visible/sized): a widget's very first layout pass isn't
/// synchronous (`init` calls `populate_month_grid`/`populate_day_view` before the
/// window has been through the Wayland compositor's first round-trip, so
/// `grid.height()`/`overlay.width()` still read 0 at that point) — and, separately, a
/// *hidden* widget (`day_overlay` sits inside `day_view_container`, invisible whenever
/// Month is the active view) never gets allocated a size at all until it's shown, so
/// `AppMsg::SetView` calls this again every time Day view is switched into, not just
/// once at startup. Both dimensions are checked (not just width) because different
/// callers depend on different ones — `day_overlay`'s column layout depends on width,
/// but the month grid's `compute_max_visible_events` depends on its parent's height.
fn poll_for_real_width_then_resize<W: IsA<gtk4::Widget> + Clone>(widget: &W, sender: &ComponentSender<App>) {
    let widget: gtk4::Widget = widget.clone().upcast();
    let sender = sender.clone();
    let mut attempts_left = 40; // ~2s at 50ms, generous for a slow compositor
    gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        attempts_left -= 1;
        if widget.width() > 0 && widget.height() > 0 {
            sender.input(AppMsg::Resize);
            return gtk4::glib::ControlFlow::Break;
        }
        if attempts_left <= 0 {
            return gtk4::glib::ControlFlow::Break;
        }
        gtk4::glib::ControlFlow::Continue
    });
}

/// Fills the sidebar with one section per connected account (mirroring the Google
/// Calendar PWA's "My calendars" / "Other calendars" sidebar, DESIGN_SPEC.md §10),
/// each listing that account's calendars with a checkbox bound to its `is_visible`
/// flag. Toggling a checkbox sends `AppMsg::ToggleCalendarVisibility`. A "show all"
/// button sits atop the whole list (`AppMsg::ShowAllCalendars`) and another sits on
/// each account's header row, scoped to that account
/// (`AppMsg::ShowAllCalendarsForAccount`) — the opposite of a calendar's own "Display
/// this only" action, which this list has no other way to undo.
fn populate_sidebar(
    container: &gtk4::Box,
    accounts: &[Account],
    calendars: &[CalendarSummary],
    sender: &ComponentSender<App>,
) {
    clear_children(container);

    if accounts.is_empty() {
        let empty = gtk4::Label::new(Some("No accounts connected yet."));
        empty.add_css_class("dim-label");
        empty.set_halign(gtk4::Align::Start);
        empty.set_wrap(true);
        container.append(&empty);
        return;
    }

    let list_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    list_header.add_css_class("sidebar-list-header");
    let list_title = gtk4::Label::new(Some("Calendars"));
    list_title.add_css_class("sidebar-section-header");
    list_title.set_halign(gtk4::Align::Start);
    list_title.set_hexpand(true);
    list_header.append(&list_title);
    let sender_for_all = sender.clone();
    list_header.append(&sidebar_pill_button("Show all", "Show all calendars", move || {
        sender_for_all.input(AppMsg::ShowAllCalendars);
    }));
    container.append(&list_header);

    for account in accounts {
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        header.add_css_class("sidebar-section-header-row");
        let label = gtk4::Label::new(Some(account.display_name.as_deref().unwrap_or(&account.email)));
        label.add_css_class("sidebar-section-header");
        label.set_halign(gtk4::Align::Start);
        label.set_hexpand(true);
        header.append(&label);
        let account_id = account.id;
        let sender_for_account = sender.clone();
        header.append(&sidebar_pill_button("Show all", "Show all calendars in this account", move || {
            sender_for_account.input(AppMsg::ShowAllCalendarsForAccount { account_id });
        }));
        container.append(&header);

        for calendar in calendars.iter().filter(|c| c.account_id == account.id) {
            container.append(&calendar_row(calendar, sender));
        }
    }
}

/// A small pill-shaped text button for a one-click sidebar action — same `.pill`
/// style class as the toolbar's "Today" button, so it reads as the same "kind of
/// thing" as the rest of the app rather than inventing a new theme-unaware affordance.
/// Unlike `calendar_customizer_button` this opens no popover: it fires `on_click`
/// immediately.
fn sidebar_pill_button(label: &str, tooltip: &str, on_click: impl Fn() + 'static) -> gtk4::Button {
    let button = gtk4::Button::new();
    button.set_label(label);
    button.add_css_class("pill");
    button.add_css_class("sidebar-pill-button");
    button.set_valign(gtk4::Align::Center);
    button.set_tooltip_text(Some(tooltip));
    button.connect_clicked(move |_| on_click());
    button
}

fn calendar_row(calendar: &CalendarSummary, sender: &ComponentSender<App>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.add_css_class("sidebar-calendar-row");

    // A plain `CheckButton`'s native indicator ignores `border-radius` entirely in
    // this GTK4/libadwaita version (it always renders as a fully-rounded capsule,
    // radius pinned to half its own height, regardless of what CSS asks for) — so
    // the checkbox is built from a flat `ToggleButton` instead, which is a normal
    // button node and honors `border-radius` like any other widget.
    let check = gtk4::ToggleButton::new();
    check.add_css_class("flat");
    check.add_css_class("calendar-checkbox");
    check.set_icon_name("object-select-symbolic");
    check.set_valign(gtk4::Align::Center);
    check.set_tooltip_text(Some(&format!("Show or hide {}", calendar.display_name)));
    if let Some(color) = &calendar.color {
        check.add_css_class(&css_class_for_color(color));
    }
    check.set_active(calendar.is_visible);
    let calendar_id = calendar.id;
    let sender_for_check = sender.clone();
    check.connect_toggled(move |btn| {
        sender_for_check.input(AppMsg::ToggleCalendarVisibility {
            calendar_id,
            visible: btn.is_active(),
        });
    });
    row.append(&check);

    let label = gtk4::Label::new(Some(&calendar.display_name));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&label);

    row.append(&calendar_customizer_button(calendar, sender));

    row
}

/// The calendar customizer's color-swatch grid (DESIGN_SPEC.md §10, reference
/// screenshot) — Google Calendar's 11 built-in event colors plus one extra so the
/// grid fills a clean 2-row-by-6 layout.
// (hex, friendly name) — the name is what a swatch's tooltip actually shows; a bare
// hex code in a tooltip isn't "info" a user can act on the way Google Calendar's own
// named palette is.
const CALENDAR_COLOR_PALETTE: &[(&str, &str)] = &[
    ("#d50000", "Tomato"),
    ("#e67c73", "Flamingo"),
    ("#f4511e", "Tangerine"),
    ("#f6bf26", "Banana"),
    ("#33b679", "Sage"),
    ("#0b8043", "Basil"),
    ("#039be5", "Peacock"),
    ("#3f51b5", "Blueberry"),
    ("#7986cb", "Lavender"),
    ("#8e24aa", "Grape"),
    ("#616161", "Graphite"),
    ("#795548", "Cocoa"),
];

/// The "⋮" button on a sidebar calendar row: a popover mirroring the Google Calendar
/// PWA's per-calendar menu (reference screenshot) — "Display this only" (isolates this
/// calendar's visibility via `AppMsg::ShowOnlyCalendar`), "Settings and sharing" (not
/// built yet, so left visible-but-disabled like the app's other "coming soon"
/// affordances), and a color-swatch grid bound to `AppMsg::SetCalendarColor`, with the
/// calendar's current color marked by a checkmark.
fn calendar_customizer_button(calendar: &CalendarSummary, sender: &ComponentSender<App>) -> gtk4::MenuButton {
    let button = gtk4::MenuButton::new();
    button.set_icon_name("view-more-symbolic");
    button.add_css_class("flat");
    button.add_css_class("calendar-customizer-button");
    button.set_valign(gtk4::Align::Center);
    button.set_tooltip_text(Some("Calendar options"));

    let popover = gtk4::Popover::new();
    popover.add_css_class("calendar-customizer-popover");

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    root.set_margin_all(6);
    root.set_width_request(220);

    let display_only_btn = menu_row_button("Display this only");
    {
        let sender = sender.clone();
        let calendar_id = calendar.id;
        let popover = popover.clone();
        display_only_btn.connect_clicked(move |_| {
            sender.input(AppMsg::ShowOnlyCalendar { calendar_id });
            popover.popdown();
        });
    }
    root.append(&display_only_btn);

    let settings_btn = menu_row_button("Settings and sharing");
    settings_btn.set_sensitive(false);
    settings_btn.set_tooltip_text(Some("Calendar settings (coming soon)"));
    root.append(&settings_btn);

    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let colors_grid = gtk4::Grid::new();
    colors_grid.set_row_spacing(6);
    colors_grid.set_column_spacing(6);
    colors_grid.set_margin_top(8);
    colors_grid.set_halign(gtk4::Align::Center);

    for (index, &(color, name)) in CALENDAR_COLOR_PALETTE.iter().enumerate() {
        let swatch = gtk4::Button::new();
        swatch.add_css_class("flat");
        swatch.add_css_class("circular");
        swatch.add_css_class("color-swatch");
        swatch.add_css_class(&css_class_for_color(color));
        swatch.set_tooltip_text(Some(name));
        if calendar.color.as_deref() == Some(color) {
            swatch.add_css_class("color-swatch-selected");
            swatch.set_icon_name("object-select-symbolic");
        }

        let sender = sender.clone();
        let calendar_id = calendar.id;
        let color = color.to_string();
        let popover = popover.clone();
        swatch.connect_clicked(move |_| {
            sender.input(AppMsg::SetCalendarColor {
                calendar_id,
                color: color.clone(),
            });
            popover.popdown();
        });

        colors_grid.attach(&swatch, (index % 6) as i32, (index / 6) as i32, 1, 1);
    }
    root.append(&colors_grid);

    popover.set_child(Some(&root));
    button.set_popover(Some(&popover));

    button
}

/// A full-width, left-aligned flat button for a popover's text menu items — the
/// "Display this only" / "Settings and sharing" rows in `calendar_customizer_button`,
/// which (unlike a plain `Button::with_label`) need their label pinned to the start
/// rather than centered so they read like the reference screenshot's menu.
fn menu_row_button(text: &str) -> gtk4::Button {
    let label = gtk4::Label::new(Some(text));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);

    let button = gtk4::Button::new();
    button.set_child(Some(&label));
    button.add_css_class("flat");
    button.add_css_class("customizer-menu-row");
    button
}

/// Same as `menu_row_button`, but appends a right-aligned keyboard-shortcut badge
/// after the label — reuses `build_shortcuts_content`'s `.shortcut-key` styling so a
/// menu row's accelerator hint reads as the same "kind of thing" as the `?` shortcuts
/// window's key badges, rather than inventing a second visual language for the same
/// concept. Used by `build_view_switcher_popover` for the Month/Day rows, whose
/// single-letter accelerators (`install_view_shortcuts`) are otherwise undiscoverable
/// from the menu itself.
fn menu_row_button_with_hotkey(text: &str, key: &str) -> gtk4::Button {
    let label = gtk4::Label::new(Some(text));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);

    let key_label = gtk4::Label::new(Some(key));
    key_label.add_css_class("shortcut-key");

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&label);
    row.append(&key_label);

    let button = gtk4::Button::new();
    button.set_child(Some(&row));
    button.add_css_class("flat");
    button.add_css_class("customizer-menu-row");
    button
}

/// Wraps `sender` into the `on_pick` callback `populate_mini_calendar` and
/// `build_quick_jump_popover` take, so the sidebar's mini calendar — which should page
/// the *main* view — can share that day-grid/quick-jump-popover code with the edit
/// dialog's date pickers (`date_picker`), which instead just update their own field.
fn jump_to_date_callback(sender: &ComponentSender<App>) -> Rc<dyn Fn(NaiveDate)> {
    let sender = sender.clone();
    Rc::new(move |date| sender.input(AppMsg::JumpToDate(date)))
}

/// Wraps `sender` into the `on_range_pick` callback `populate_mini_calendar` takes —
/// the sidebar-only counterpart of `jump_to_date_callback` for a completed drag-select
/// (`wire_mini_calendar_drag_select`), sent as `AppMsg::SetCustomViewDates`.
fn range_pick_callback(sender: &ComponentSender<App>) -> Rc<dyn Fn(Vec<NaiveDate>)> {
    let sender = sender.clone();
    Rc::new(move |dates| sender.input(AppMsg::SetCustomViewDates(dates)))
}

/// Fills the sidebar's mini-month date navigator (DESIGN_SPEC.md §10): a compact
/// Sunday-first month grid with no event rendering, just clickable day buttons.
/// Shares `display_month`/`today` with the main month grid, so paging or jumping
/// either one keeps both in sync. Clicking any day — including a padding day from
/// the adjacent month — calls `on_pick` with that date. The title button's popover
/// (`build_quick_jump_popover`) is rebuilt on every call so its year spinner and
/// highlighted month always start from whatever month is currently displayed, rather
/// than staying stuck on the value from whenever the popover was first constructed.
/// Also reused, with a different `on_pick`, by the edit dialog's `date_picker` — which
/// passes `None` for `on_range_pick` so its day grid stays single-click-only; the
/// sidebar instance passes `Some` to additionally wire each day button for drag-select
/// (`wire_mini_calendar_drag_select`). `selected_range` paints `.mini-calendar-day-in-range`
/// on whichever dates it names from the moment the grid is built — so a completed
/// drag-select (`App::refresh` passing `custom_view_dates` while `ViewMode::Custom` is
/// active) stays highlighted after the drag ends and the grid is rebuilt, not just
/// during the live drag itself (`date_picker` passes `&[]`, having no such state).
fn populate_mini_calendar(
    grid: &gtk4::Grid,
    title_button: &gtk4::MenuButton,
    display_month: NaiveDate,
    today: NaiveDate,
    on_pick: &Rc<dyn Fn(NaiveDate)>,
    on_jump_month: &Rc<dyn Fn(NaiveDate)>,
    on_range_pick: Option<&Rc<dyn Fn(Vec<NaiveDate>)>>,
    selected_range: &[NaiveDate],
) {
    const WEEKDAYS: [&str; 7] = ["S", "M", "T", "W", "T", "F", "S"];

    clear_children(grid);
    title_button.set_label(&display_month.format("%B %Y").to_string());
    title_button.set_popover(Some(&build_quick_jump_popover(display_month, on_jump_month)));

    for (col, day) in WEEKDAYS.iter().enumerate() {
        let label = gtk4::Label::new(Some(day));
        label.add_css_class("mini-weekday-label");
        grid.attach(&label, col as i32, 0, 1, 1);
    }

    let year = display_month.year();
    let month = display_month.month();
    let first_of_month = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month");
    let days_in_month = days_in_month(year, month);
    let leading = first_of_month.weekday().num_days_from_sunday();
    let total_cells = leading + days_in_month;
    let rows = total_cells.div_ceil(7);
    let grid_start = first_of_month - Duration::days(leading as i64);

    // Populated as day buttons are created below, then shared into every button's drop
    // target (`on_range_pick` only) so a hovered cell can repaint the whole in-progress
    // range across every cell it spans, not just itself. Rebuilt fresh each call, same
    // lifetime as the grid it populates.
    let cells: Rc<RefCell<Vec<(NaiveDate, gtk4::Button)>>> = Rc::new(RefCell::new(Vec::new()));

    for cell_index in 0..(rows * 7) {
        let date = grid_start + Duration::days(cell_index as i64);
        let col = (cell_index % 7) as i32;
        let row = (cell_index / 7) as i32 + 1;

        let button = gtk4::Button::with_label(&date.day().to_string());
        button.add_css_class("flat");
        button.add_css_class("mini-calendar-day");
        if date.month() != month {
            button.add_css_class("dim-label");
        }
        if date == today {
            button.add_css_class("mini-calendar-today");
        }
        if selected_range.contains(&date) {
            button.add_css_class("mini-calendar-day-in-range");
        }

        let on_pick_cloned = on_pick.clone();
        button.connect_clicked(move |_| on_pick_cloned(date));

        if let Some(on_range_pick) = on_range_pick {
            wire_mini_calendar_drag_select(&button, date, &cells, on_range_pick);
            cells.borrow_mut().push((date, button.clone()));
        }

        grid.attach(&button, col, row, 1, 1);
    }
}

/// Toggles `.mini-calendar-day-in-range` across every button in `cells` to match
/// `range` — called on every `connect_motion` tick of a mini calendar drag-select so
/// the highlighted span always matches `range`'s current extent, not just the button
/// under the pointer.
fn highlight_mini_calendar_range(cells: &Rc<RefCell<Vec<(NaiveDate, gtk4::Button)>>>, range: &[NaiveDate]) {
    for (cell_date, button) in cells.borrow().iter() {
        if range.contains(cell_date) {
            button.add_css_class("mini-calendar-day-in-range");
        } else {
            button.remove_css_class("mini-calendar-day-in-range");
        }
    }
}

/// Clears both drag-select CSS classes off every button in `cells` — called when a
/// drag ends, whether committed (`connect_drop`, about to be wiped anyway by the
/// resulting `refresh`) or cancelled (`connect_drag_cancel`, which needs it since no
/// refresh follows).
fn clear_mini_calendar_range_highlight(cells: &Rc<RefCell<Vec<(NaiveDate, gtk4::Button)>>>) {
    for (_, button) in cells.borrow().iter() {
        button.remove_css_class("mini-calendar-day-selected");
        button.remove_css_class("mini-calendar-day-in-range");
    }
}

/// Wires one mini calendar day button as both a drag origin and a drop target for
/// drag-select (`populate_mini_calendar`'s `on_range_pick`) — mirrors
/// `wire_month_event_drag_source`/`wire_month_cell_drop_target`'s native
/// `gtk4::DragSource`/`gtk4::DropTarget` pattern (content: the date, as its
/// `num_days_from_ce` i64) rather than a hand-rolled `GestureDrag`, for the same
/// reason given there: a native `DragSource` coexists with the button's own
/// `connect_clicked` with no extra arbitration, so a plain click still jumps a single
/// day exactly as before. Unlike the Month view's single-cell highlight, `DropTarget`'s
/// preloaded value (`set_preload`) lets `connect_motion` recompute and repaint the
/// *whole* capped range (`capped_date_range`) on every cell entered, not just the one
/// under the pointer — the "select up to `MINI_CALENDAR_DRAG_MAX_DAYS` side by side"
/// affordance. Every day button is wired as both source and target, since a drag can
/// start on any cell and later hover any other (including back over its own origin).
fn wire_mini_calendar_drag_select(
    button: &gtk4::Button,
    date: NaiveDate,
    cells: &Rc<RefCell<Vec<(NaiveDate, gtk4::Button)>>>,
    on_range_pick: &Rc<dyn Fn(Vec<NaiveDate>)>,
) {
    let drag_source = gtk4::DragSource::new();
    drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
    let origin_days = date.num_days_from_ce() as i64;
    drag_source.connect_prepare(move |_, _, _| Some(gtk4::gdk::ContentProvider::for_value(&origin_days.to_value())));
    {
        let button = button.clone();
        drag_source.connect_drag_begin(move |_, _| {
            button.add_css_class("mini-calendar-day-selected");
        });
    }
    {
        let cells = cells.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            clear_mini_calendar_range_highlight(&cells);
        });
    }
    {
        let cells = cells.clone();
        drag_source.connect_drag_cancel(move |_, _, _| {
            clear_mini_calendar_range_highlight(&cells);
            false
        });
    }
    button.add_controller(drag_source);

    let drop_target = gtk4::DropTarget::new(i64::static_type(), gtk4::gdk::DragAction::MOVE);
    drop_target.set_preload(true);
    {
        let cells = cells.clone();
        drop_target.connect_motion(move |target, _, _| {
            if let Some(origin_days) = target.value().and_then(|v| v.get::<i64>().ok()) {
                if let Some(origin) = NaiveDate::from_num_days_from_ce_opt(origin_days as i32) {
                    highlight_mini_calendar_range(&cells, &capped_date_range(origin, date, MINI_CALENDAR_DRAG_MAX_DAYS));
                }
            }
            gtk4::gdk::DragAction::MOVE
        });
    }
    {
        let cells = cells.clone();
        let on_range_pick = on_range_pick.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            clear_mini_calendar_range_highlight(&cells);
            let Ok(origin_days) = value.get::<i64>() else { return false };
            let Some(origin) = NaiveDate::from_num_days_from_ce_opt(origin_days as i32) else { return false };
            let range = capped_date_range(origin, date, MINI_CALENDAR_DRAG_MAX_DAYS);
            if range.len() < 2 {
                return false;
            }
            on_range_pick(range);
            true
        });
    }
    button.add_controller(drop_target);
}

/// Builds the popover the mini calendar's month/year title button opens — a year
/// spin button (directly typable, not just steppable) plus a 3×4 grid of month
/// buttons — so jumping several months or years away takes one click instead of
/// repeatedly paging the prev/next arrows. Picking a month calls `on_jump_month` with
/// the 1st of the chosen year/month (matching `shift_month`'s convention that a
/// "current date" only needs to identify *which* month is displayed) and closes only
/// *this* popover — deliberately a separate callback from `populate_mini_calendar`'s
/// `on_pick` (an actual day click), since for `date_picker` these mean different
/// things: picking a day commits a value and closes the whole date field's popover,
/// while jumping months should only renavigate the day grid underneath, leaving that
/// popover open. The currently-displayed month is highlighted with the
/// `suggested-action` style class Adwaita already provides.
fn build_quick_jump_popover(current: NaiveDate, on_jump_month: &Rc<dyn Fn(NaiveDate)>) -> gtk4::Popover {
    const MONTH_NAMES: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    let popover = gtk4::Popover::new();
    popover.add_css_class("mini-calendar-jump-popover");

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    root.set_margin_all(10);
    root.set_width_request(200);

    let year_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let year_label = gtk4::Label::new(Some("Year"));
    year_label.set_halign(gtk4::Align::Start);
    year_label.set_hexpand(true);
    year_row.append(&year_label);

    let year_adjustment = gtk4::Adjustment::new(current.year() as f64, 1.0, 9999.0, 1.0, 10.0, 0.0);
    let year_spin = gtk4::SpinButton::new(Some(&year_adjustment), 1.0, 0);
    year_row.append(&year_spin);
    root.append(&year_row);

    let months_grid = gtk4::Grid::new();
    months_grid.set_row_homogeneous(true);
    months_grid.set_column_homogeneous(true);
    months_grid.set_row_spacing(4);
    months_grid.set_column_spacing(4);

    for (index, name) in MONTH_NAMES.iter().enumerate() {
        let month = (index + 1) as u32;
        let button = gtk4::Button::with_label(name);
        button.add_css_class("flat");
        if month == current.month() {
            button.add_css_class("suggested-action");
        }

        let on_jump_month = on_jump_month.clone();
        let year_spin = year_spin.clone();
        let popover_weak = popover.downgrade();
        button.connect_clicked(move |_| {
            let year = year_spin.value_as_int();
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, 1) {
                on_jump_month(date);
            }
            if let Some(popover) = popover_weak.upgrade() {
                popover.popdown();
            }
        });

        months_grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
    }
    root.append(&months_grid);

    popover.set_child(Some(&root));
    popover
}

/// Fills a `gtk4::Grid` with a flat, borderless-but-hairlined Sunday-first month
/// calendar (DESIGN_SPEC.md §10's Month view): complete weeks, padded with the
/// tail/head of the adjacent months so every row is a full 7 days, weekday
/// abbreviations folded into the first row's cells rather than a separate header row,
/// and as many compact `<dot> <time> <title>` rows per day as fit the grid's current
/// height (see `compute_max_visible_events`) before collapsing the rest into a
/// "N more" line. Events are grouped by `DisplayEvent::start_date()` client-side
/// rather than via a per-day SQL query — see `calendarchy_service_calendar::query`.
/// Safe to call repeatedly (e.g. after a sidebar toggle changes visible events, or
/// the header bar's Today/prev/next controls change which month is displayed) — it
/// clears any previously-attached cells first. `display_month` picks which month is
/// rendered; `today` is the real calendar date, used only for the today-badge — kept
/// separate so paging away from the current month doesn't mis-badge some other day.
fn populate_month_grid(
    grid: &gtk4::Grid,
    display_month: NaiveDate,
    today: NaiveDate,
    events: &[DisplayEvent],
    ctx: &EventCtx,
) {
    const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

    clear_children(grid);

    let mut events_by_day: HashMap<&str, Vec<&DisplayEvent>> = HashMap::new();
    for event in events {
        events_by_day.entry(event.start_date()).or_default().push(event);
    }

    let year = display_month.year();
    let month = display_month.month();
    let first_of_month = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month");
    let days_in_month = days_in_month(year, month);
    let leading = first_of_month.weekday().num_days_from_sunday();
    let total_cells = leading + days_in_month;
    let rows = total_cells.div_ceil(7);
    let grid_start = first_of_month - Duration::days(leading as i64);
    let max_visible_events = compute_max_visible_events(grid, rows);

    for cell_index in 0..(rows * 7) {
        let date = grid_start + Duration::days(cell_index as i64);
        let col = (cell_index % 7) as i32;
        let row = (cell_index / 7) as i32;
        let in_current_month = date.month() == month;

        let cell = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        cell.add_css_class("month-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);
        wire_month_cell_drop_target(&cell, date, ctx);

        if row == 0 {
            let weekday_label = gtk4::Label::new(Some(WEEKDAYS[col as usize]));
            weekday_label.add_css_class("weekday-label");
            weekday_label.set_halign(gtk4::Align::Start);
            cell.append(&weekday_label);
        }

        // Only the 1st of a month is disambiguated with its month name (matching how
        // Google/Apple Calendar label month boundaries) — every other day is a bare
        // number, whether or not it belongs to the currently-displayed month.
        let date_text = if date.day() == 1 {
            date.format("%b %-d").to_string()
        } else {
            date.day().to_string()
        };
        let date_label = gtk4::Label::new(Some(&date_text));
        date_label.set_halign(gtk4::Align::Start);
        if !in_current_month {
            date_label.add_css_class("dim-label");
        }
        if date == today {
            date_label.add_css_class("today-badge");
        }
        cell.append(&date_label);

        let date_key = date.format("%Y-%m-%d").to_string();
        if let Some(day_events) = events_by_day.get(date_key.as_str()) {
            for event in day_events.iter().take(max_visible_events) {
                let row = event_row(event, ctx.time_format);
                wire_event_click(&row, event, ctx);
                wire_month_event_drag_source(&row, event);
                cell.append(&row);
            }
            if day_events.len() > max_visible_events {
                let more = gtk4::Label::new(Some(&format!("{} more", day_events.len() - max_visible_events)));
                more.set_halign(gtk4::Align::Start);
                more.add_css_class("more-label");
                more.set_cursor_from_name(Some("pointer"));
                let all_day_events: Vec<DisplayEvent> = day_events.iter().map(|e| (*e).clone()).collect();
                wire_day_summary_hover(&more, date, all_day_events, ctx);
                cell.append(&more);
            }
        }

        grid.attach(&cell, col, row, 1, 1);
    }
}

/// How many event rows fit in a day cell before the rest collapse into an "N more"
/// line — computed from the grid's actual allocated height (divided evenly across
/// `week_rows`, since `month_grid` is row-homogeneous) rather than a fixed constant,
/// so a taller window, or a month with fewer weeks, shows correspondingly more
/// events instead of always capping at some guess. Rather than hand-summing label
/// heights and CSS padding/spacing constants (which drifted out of sync with the
/// real `.month-cell` layout in practice — a shrink could still overflow the
/// viewport by a few px and clip a whole week row), this builds real,
/// identically-styled probe cells and asks GTK to measure them directly, so the
/// result can't drift from whatever `.month-cell`'s actual CSS/spacing is. Falls
/// back to a conservative default before the grid has been laid out once
/// (`grid.height()` reads 0 until then) — `init`'s callers correct this by
/// re-running once real layout has happened.
fn compute_max_visible_events(grid: &gtk4::Grid, week_rows: u32) -> usize {
    const FALLBACK: usize = 4;
    const MAX_CANDIDATE: usize = 20; // generous ceiling; no real day needs more

    // Measured off the parent (the `.card` `ScrolledWindow`), not the grid
    // itself: a `ScrolledWindow` can allocate its child *more* height than is
    // actually visible — that's how scrolling works — so `grid.height()` tracks
    // the content's full demanded size and never shrinks below whatever was last
    // rendered. Reading the viewport's real allocated height here is what lets a
    // vertical shrink actually reduce the per-cell event count, instead of the
    // grid staying oversized and just scrolling its later week rows out of view.
    let viewport_height = grid.parent().map(|viewport| viewport.height()).unwrap_or_else(|| grid.height());
    if viewport_height <= 0 || week_rows == 0 {
        return FALLBACK;
    }
    let row_height = viewport_height / week_rows as i32;

    let probe_event = DisplayEvent {
        id: 0,
        title: "Probe".into(),
        description: None,
        location: None,
        start: String::new(),
        end: String::new(),
        all_day: true,
        color: None,
        calendar_name: String::new(),
        is_recurring: false,
        has_video_call: false,
        is_private: false,
        self_response_status: None,
        reminder_count: 0,
        other_attendee_count: 0,
        other_attendee_names: Vec::new(),
        attachment_count: 0,
    };

    // A row-0-style cell (weekday label + date label + `n` probe events + a
    // reserved "more" line, same CSS classes and child spacing as the real cells
    // `populate_month_grid` builds) — measured, not estimated, so it matches
    // whatever `.month-cell`'s actual padding/spacing/fonts produce.
    let fits = |n: usize| -> bool {
        let cell = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        cell.add_css_class("month-cell");

        let weekday_label = gtk4::Label::new(Some("SUN"));
        weekday_label.add_css_class("weekday-label");
        cell.append(&weekday_label);

        cell.append(&gtk4::Label::new(Some("30")));

        for _ in 0..n {
            // `probe_event.all_day` is `true`, so `event_row` never renders a time
            // prefix here — the `TimeFormat` passed is unused, kept fixed rather than
            // threaded all the way into this measurement-only closure.
            cell.append(&event_row(&probe_event, TimeFormat::TwentyFourHour));
        }

        let more = gtk4::Label::new(Some("9 more"));
        more.add_css_class("more-label");
        cell.append(&more);

        cell.measure(gtk4::Orientation::Vertical, -1).1 <= row_height
    };

    let mut n = 0;
    while n < MAX_CANDIDATE && fits(n + 1) {
        n += 1;
    }
    n.max(1)
}

/// One compact `<dot> <time> <title>` row for an event chip in a month cell. Rendering
/// only — kept free of any click handling so `compute_max_visible_events` can build an
/// unwired probe row purely for measurement; `wire_event_click` adds the interactive
/// part for rows that actually go on the grid.
/// A small cluster of at-a-glance icons for one event — shared by `event_row` (Month
/// view) and `day_event_block` (Day view) so the two chip renderers don't duplicate
/// this icon-selection logic. Returns `None` when none of the conditions below hold,
/// so a plain event with nothing notable renders exactly as it did before this existed
/// (no empty row taking up space). Each icon reuses `"dim-label"` — the same muted-icon
/// treatment `detail_row`'s leading icon already uses elsewhere in this file — and
/// carries a tooltip, since a bare tiny symbolic icon isn't self-explanatory on its own.
///
/// The video-call icon is informational only here (not clickable) — a real "Join"
/// action lives in `show_event_popover` instead (via `EventDetail::hangout_link`),
/// since nesting a clickable control inside the chip's own click gesture isn't worth
/// the complexity for what a popover already does better. `attachment_count` isn't
/// surfaced as its own chip icon (kept out to limit how many badges a busy event can
/// accumulate at once) — it's still on `DisplayEvent` for the popover, per the same
/// rationale.
fn event_badge_row(event: &DisplayEvent) -> Option<gtk4::Box> {
    let mut icons: Vec<(&'static str, String)> = Vec::new(); // (icon name, tooltip)

    if event.is_recurring {
        icons.push(("media-playlist-repeat-symbolic", "Recurring event".to_string()));
    }
    if event.has_video_call {
        icons.push(("camera-web-symbolic", "Has a video call".to_string()));
    }
    if event.is_private {
        icons.push(("channel-secure-symbolic", "Private".to_string()));
    }
    if event.reminder_count > 0 {
        icons.push(("alarm-symbolic", "Has a reminder".to_string()));
    }
    if let Some(location) = event.location.as_deref().filter(|s| !s.is_empty()) {
        icons.push(("mark-location-symbolic", location.to_string()));
    }
    match event.other_attendee_count {
        0 => {}
        1 => icons.push(("avatar-default-symbolic", guest_badge_tooltip(1, &event.other_attendee_names))),
        n => icons.push(("system-users-symbolic", guest_badge_tooltip(n, &event.other_attendee_names))),
    }

    if icons.is_empty() {
        return None;
    }

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 3);
    row.add_css_class("event-badge-row");
    for (icon_name, tooltip) in icons {
        let icon = gtk4::Image::from_icon_name(icon_name);
        icon.add_css_class("dim-label");
        icon.set_tooltip_text(Some(&tooltip));
        row.append(&icon);
    }
    Some(row)
}

/// How many attendee names `guest_badge_tooltip` spells out before collapsing the
/// rest into "and N more" — same "cap it, don't just dump the whole list" idea as
/// `MAX_VISIBLE_GUESTS` in the popover's own guest list, just a tighter cap since a
/// chip's tooltip is meant to be a glance, not a second guest list.
const MAX_GUEST_NAMES_IN_TOOLTIP: usize = 3;

/// Builds the guest badge's tooltip: a plain count when no names came back from the
/// query (shouldn't normally happen once `count > 0`, but `other_attendee_names` is
/// populated by a separate `LEFT JOIN` from the count, so treat a mismatch
/// defensively rather than panicking or under-reporting), otherwise the first few
/// names followed by "and N more" once there are more than
/// `MAX_GUEST_NAMES_IN_TOOLTIP`.
fn guest_badge_tooltip(count: i64, names: &[String]) -> String {
    if names.is_empty() {
        return if count == 1 { "1 guest".to_string() } else { format!("{count} guests") };
    }
    let shown: Vec<&str> = names.iter().take(MAX_GUEST_NAMES_IN_TOOLTIP).map(String::as_str).collect();
    let mut tooltip = shown.join(", ");
    let remaining = names.len().saturating_sub(shown.len());
    if remaining > 0 {
        tooltip.push_str(&format!(", and {remaining} more"));
    }
    tooltip
}

fn event_row(event: &DisplayEvent, time_format: TimeFormat) -> gtk4::Box {
    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    row_box.add_css_class("event-row");
    if event.self_response_status == Some(AttendeeResponseStatus::Declined) {
        row_box.add_css_class("event-declined");
    }
    row_box.set_cursor_from_name(Some("pointer"));

    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("event-dot");
    dot.set_valign(gtk4::Align::Center);
    dot.set_tooltip_text(Some(&event.calendar_name));
    if let Some(color) = &event.color {
        dot.add_css_class(&css_class_for_color(color));
    }
    row_box.append(&dot);

    let time_prefix = if event.all_day {
        String::new()
    } else {
        DateTime::parse_from_rfc3339(&event.start)
            .map(|dt| format!("{} ", format_clock(dt.time(), time_format)))
            .unwrap_or_default()
    };
    let title_label = gtk4::Label::new(Some(&format!("{time_prefix}{}", event.title)));
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row_box.append(&title_label);

    if let Some(badges) = event_badge_row(event) {
        row_box.append(&badges);
    }

    row_box
}

/// One bar in the Day/5-day view's all-day strip (`populate_day_header`, positioned by
/// `layout_all_day_events`) — a solid-colored, white-text card matching the Google
/// Calendar PWA's own all-day bars, rather than `event_row`'s small dot-plus-label chip
/// (which the month grid and event popovers still use). Reuses `day_event_block`'s own
/// `.day-event-block` CSS class (background/border/radius/declined-opacity already
/// defined there) plus the same `css_class_for_color` classes every colored dot in this
/// file draws from, so no new color CSS is needed. `clipped_start`/`clipped_end` (from
/// `AllDayEventLayout`) turn that edge into an arrow head when true (Google Calendar's
/// own cue that an event's real range continues past the currently visible dates): the
/// body drops its rounded corners and border on that side
/// (`.day-all-day-event-clipped-*`) and an `all_day_arrow_head` painted in the same
/// color is placed beside it. GTK CSS can't clip a widget to a non-rectangular shape,
/// which is why the arrow is a separate drawn sibling rather than part of the body —
/// and why the returned widget is a wrapper `Box` around body + arrows rather than the
/// body itself: the wrapper carries the click target, the pointer cursor, and the
/// hover/declined opacity (`.day-all-day-bar`), so the arrow head fades and highlights
/// in lockstep with the body instead of reading as a separate piece. Unlike
/// `day_event_block`, this needs no absolute positioning: `populate_day_header` attaches
/// it straight into a `Grid` cell (or cell span), which sizes it for us.
fn all_day_event_bar(event: &DisplayEvent, clipped_start: bool, clipped_end: bool) -> gtk4::Box {
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    bar.add_css_class("day-all-day-bar");
    if event.self_response_status == Some(AttendeeResponseStatus::Declined) {
        bar.add_css_class("event-declined");
    }
    bar.set_cursor_from_name(Some("pointer"));

    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    body.add_css_class("day-event-block");
    body.add_css_class("day-all-day-event");
    if let Some(color) = &event.color {
        body.add_css_class(&css_class_for_color(color));
    }
    body.set_hexpand(true);

    if clipped_start {
        body.add_css_class("day-all-day-event-clipped-start");
        bar.append(&all_day_arrow_head(event.color.as_deref(), false));
    }

    let subject = gtk4::Label::new(Some(&event.title));
    subject.add_css_class("day-event-subject");
    subject.set_halign(gtk4::Align::Start);
    subject.set_hexpand(true);
    subject.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    body.append(&subject);

    if let Some(badges) = event_badge_row(event) {
        body.append(&badges);
    }

    bar.append(&body);

    if clipped_end {
        body.add_css_class("day-all-day-event-clipped-end");
        bar.append(&all_day_arrow_head(event.color.as_deref(), true));
    }

    bar
}

/// Width of the arrow head `all_day_arrow_head` draws at a clipped all-day bar's edge.
const ALL_DAY_ARROW_WIDTH_PX: i32 = 10;

/// The arrow head `all_day_event_bar` puts at a clipped edge: a `DrawingArea` as tall
/// as the bar, filled with a triangle whose flat side sits flush against the bar body
/// and whose tip points away from it (`points_right` picks the side), in the event's
/// own `color` (the same hex `css_class_for_color` registers as the body's background,
/// parsed here since a drawn shape can't take a CSS class; a color-less event gets a
/// neutral grey). The two slanted edges get the same quarter-alpha black hairline as
/// `.day-event-block`'s border so the outline continues unbroken around the point.
fn all_day_arrow_head(color: Option<&str>, points_right: bool) -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.add_css_class("day-all-day-arrow");
    area.set_content_width(ALL_DAY_ARROW_WIDTH_PX);
    area.set_valign(gtk4::Align::Fill);
    let fill = color
        .and_then(|hex| gtk4::gdk::RGBA::parse(hex).ok())
        .unwrap_or_else(|| gtk4::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
    area.set_draw_func(move |_, cr, width, height| {
        let (w, h) = (f64::from(width), f64::from(height));
        let (base_x, tip_x) = if points_right { (0.0, w) } else { (w, 0.0) };
        cr.move_to(base_x, 0.0);
        cr.line_to(tip_x, h / 2.0);
        cr.line_to(base_x, h);
        cr.close_path();
        cr.set_source_rgba(f64::from(fill.red()), f64::from(fill.green()), f64::from(fill.blue()), f64::from(fill.alpha()));
        let _ = cr.fill();

        // Hairline on the two slanted edges only — the flat edge meets the body's own
        // fill. Inset half a pixel so a 1px stroke isn't clipped at the area's bounds.
        let tip_inset = if points_right { tip_x - 0.5 } else { tip_x + 0.5 };
        cr.move_to(base_x, 0.5);
        cr.line_to(tip_inset, h / 2.0);
        cr.line_to(base_x, h - 0.5);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
        cr.set_line_width(1.0);
        let _ = cr.stroke();
    });
    area
}

/// Max gap `wire_event_click`/`install_day_event_drag` wait after a first click before
/// giving up on it turning into a double-click and actually popping `show_event_popover`
/// open — both defer that popover by this long (`gtk4::glib::timeout_add_local_once`,
/// same debounce idiom `install_sidebar_resize_persistence` uses for its own
/// cancel-and-reschedule timer) so a genuine double-click can cancel the pending
/// popover and call `open_event_editor` instead, with the popover never having been
/// shown at all. 400ms matches the default `gtk-double-click-time` most GTK apps ship
/// with.
const EVENT_DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// Wires an on-grid event row (built by `event_row`) so a single click pops up
/// `show_event_popover` anchored to it — the "nice summary with other controls" the
/// month grid didn't have before, mirroring how the Google Calendar PWA opens an
/// event's detail card from its month-view chip — while a double-click skips the
/// popover and jumps straight to `open_event_editor`, matching how double-clicking an
/// event opens it for editing in Google Calendar/most desktop calendar apps.
fn wire_event_click(row: &gtk4::Box, event: &DisplayEvent, ctx: &EventCtx) {
    let gesture = gtk4::GestureClick::new();
    let anchor = row.clone();
    let event = event.clone();
    let ctx = ctx.clone();
    // The popover from a first press is deferred by `EVENT_DOUBLE_CLICK_WINDOW` rather
    // than shown immediately: GTK's `GestureClick` fires `pressed` once per physical
    // press (`n_press == 1` for the first, `n_press == 2` for the second of a
    // double-click), so showing the popover synchronously on every press would make it
    // visibly flash open before the edit dialog replaced it. Deferring lets the
    // `n_press >= 2` branch cancel that pending popover outright, so a double-click
    // never shows it at all.
    let pending_popover: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));
    gesture.connect_pressed(move |gesture, n_press, _x, _y| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        if n_press >= 2 {
            if let Some(id) = pending_popover.take() {
                id.remove();
            }
            open_event_editor(event.id, &ctx);
            return;
        }
        if let Some(id) = pending_popover.take() {
            id.remove();
        }
        let anchor = anchor.clone();
        let event = event.clone();
        let ctx = ctx.clone();
        let pending_popover_for_timer = pending_popover.clone();
        let id = gtk4::glib::timeout_add_local_once(EVENT_DOUBLE_CLICK_WINDOW, move || {
            pending_popover_for_timer.set(None);
            show_event_popover(&anchor, &event, &ctx);
        });
        pending_popover.set(Some(id));
    });
    row.add_controller(gesture);
}

/// Hover delay before a month cell's `"N more"` label pops out `show_day_summary_popover`
/// — long enough that a mouse merely passing over the grid doesn't spawn popovers, short
/// enough to feel responsive when it's the actual target.
const DAY_SUMMARY_HOVER_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Grace period after the pointer leaves the `"more"` label (or the popover itself)
/// before it actually closes — gives the pointer time to travel from the label into the
/// popover without it flickering shut in between.
const DAY_SUMMARY_HOVER_CLOSE_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Wires a month cell's `"N more"` label so hovering it pops out `show_day_summary_popover`
/// for that day — the on-hover counterpart to `wire_event_click`'s on-click popover for a
/// single event chip, letting a busy day's full event list be previewed without switching
/// to Day view. `day_events` is the day's *entire* event list (not just the hidden tail),
/// matching how Google Calendar's own month-view overflow popup works.
///
/// Uses the same `timeout_add_local_once` + `Rc<Cell<Option<SourceId>>>` debounce idiom
/// `wire_event_click`/`install_day_event_drag` use for their click-vs-double-click timing,
/// applied here to hover-in/hover-out instead: `pending_show` defers actually building the
/// popover by `DAY_SUMMARY_HOVER_DELAY` so a pointer just passing over the label doesn't
/// spawn one, and `pending_hide` defers popping it down by `DAY_SUMMARY_HOVER_CLOSE_DELAY`
/// so moving from the label into the popover doesn't read as "left" in between. The same
/// enter/leave pair is attached to both the label and the popover itself, so the popover
/// only actually closes once the pointer has left *both*.
fn wire_day_summary_hover(label: &gtk4::Label, date: NaiveDate, day_events: Vec<DisplayEvent>, ctx: &EventCtx) {
    // Cancels a pending close (used on entering either the label or the popover — in
    // both cases the popover, if any, should just stay open).
    fn cancel_hide(pending_hide: &Rc<Cell<Option<gtk4::glib::SourceId>>>) {
        if let Some(id) = pending_hide.take() {
            id.remove();
        }
    }

    // Schedules a close after `DAY_SUMMARY_HOVER_CLOSE_DELAY`, replacing any close
    // already pending, if a popover is actually open (used on leaving either the label
    // or the popover — either one, alone, might just be the pointer passing between
    // them, so the close itself is deferred rather than immediate).
    fn schedule_hide(open: &Rc<Cell<Option<gtk4::Popover>>>, pending_hide: &Rc<Cell<Option<gtk4::glib::SourceId>>>) {
        let Some(popover) = open.take() else { return };
        open.set(Some(popover));
        cancel_hide(pending_hide);
        let open = open.clone();
        let id = gtk4::glib::timeout_add_local_once(DAY_SUMMARY_HOVER_CLOSE_DELAY, move || {
            if let Some(popover) = open.take() {
                popover.popdown();
            }
        });
        pending_hide.set(Some(id));
    }

    let open: Rc<Cell<Option<gtk4::Popover>>> = Rc::new(Cell::new(None));
    let pending_show: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));
    let pending_hide: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));

    let label_motion = gtk4::EventControllerMotion::new();
    {
        let label = label.clone();
        let ctx = ctx.clone();
        let open = open.clone();
        let pending_show = pending_show.clone();
        let pending_hide = pending_hide.clone();
        label_motion.connect_enter(move |_, _, _| {
            cancel_hide(&pending_hide);
            // Already open, or already scheduled to open: nothing more to do.
            if let Some(popover) = open.take() {
                open.set(Some(popover));
                return;
            }
            if let Some(id) = pending_show.take() {
                pending_show.set(Some(id));
                return;
            }
            let label = label.clone();
            let date_events = day_events.clone();
            let ctx = ctx.clone();
            let open = open.clone();
            let pending_hide = pending_hide.clone();
            let pending_show_for_timer = pending_show.clone();
            let id = gtk4::glib::timeout_add_local_once(DAY_SUMMARY_HOVER_DELAY, move || {
                pending_show_for_timer.set(None);
                let popover = show_day_summary_popover(&label, date, &date_events, &ctx);

                // Hovering into the popover itself keeps it open the same way hovering
                // the label does; only leaving *both* actually schedules a close.
                let popover_motion = gtk4::EventControllerMotion::new();
                {
                    let pending_hide = pending_hide.clone();
                    popover_motion.connect_enter(move |_, _, _| cancel_hide(&pending_hide));
                }
                {
                    let open = open.clone();
                    let pending_hide = pending_hide.clone();
                    popover_motion.connect_leave(move |_| schedule_hide(&open, &pending_hide));
                }
                popover.add_controller(popover_motion);

                let open_for_closed = open.clone();
                popover.connect_closed(move |_| open_for_closed.set(None));

                open.set(Some(popover));
            });
            pending_show.set(Some(id));
        });
    }
    {
        let open = open.clone();
        let pending_show = pending_show.clone();
        let pending_hide = pending_hide.clone();
        label_motion.connect_leave(move |_| {
            if let Some(id) = pending_show.take() {
                id.remove();
            }
            schedule_hide(&open, &pending_hide);
        });
    }
    label.add_controller(label_motion);
}

/// Makes a Month view event chip (built by `event_row`) draggable to another day cell —
/// paired with `wire_month_cell_drop_target` on the receiving `.month-cell`. Uses GTK4's
/// native `gtk4::DragSource`/`gtk4::DropTarget` (content: the event's `i64` id) rather
/// than a hand-rolled `GestureDrag` like the Day/5-day view's `install_day_event_drag` —
/// Month view only needs whole-cell granularity, so there's no need for that function's
/// fine-grained offset math, and a native `DragSource` coexists with `wire_event_click`'s
/// `GestureClick` on the same widget with no extra arbitration: it only claims the
/// sequence once its own drag threshold is exceeded, so a plain click still reaches the
/// click gesture normally (contrast `install_day_event_drag`'s doc comment, which
/// explains why a single custom `GestureDrag` was needed there instead). Mirrors the
/// World Clock zone-reorder drag source in Preferences (`rebuild_world_clock_zone_rows`)
/// for the drag-icon/dim-while-dragging treatment.
fn wire_month_event_drag_source(row: &gtk4::Box, event: &DisplayEvent) {
    let drag_source = gtk4::DragSource::new();
    drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
    let event_id = event.id;
    drag_source.connect_prepare(move |_, _, _| Some(gtk4::gdk::ContentProvider::for_value(&event_id.to_value())));
    {
        let row = row.clone();
        drag_source.connect_drag_begin(move |source, _drag| {
            row.add_css_class("event-row-dragging");
            let paintable = gtk4::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), row.width() / 2, row.height() / 2);
        });
    }
    {
        let row = row.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            row.remove_css_class("event-row-dragging");
        });
    }
    {
        let row = row.clone();
        drag_source.connect_drag_cancel(move |_, _, _| {
            row.remove_css_class("event-row-dragging");
            false
        });
    }
    row.add_controller(drag_source);
}

/// Accepts a drop from `wire_month_event_drag_source` on one Month view day cell —
/// highlighting it (`.month-cell-drop-target`) while a drag hovers over it, and moving
/// the dropped event to `date` (`commit_month_drag`) on release.
fn wire_month_cell_drop_target(cell: &gtk4::Box, date: NaiveDate, ctx: &EventCtx) {
    let drop_target = gtk4::DropTarget::new(i64::static_type(), gtk4::gdk::DragAction::MOVE);
    {
        let cell = cell.clone();
        drop_target.connect_enter(move |_, _, _| {
            cell.add_css_class("month-cell-drop-target");
            gtk4::gdk::DragAction::MOVE
        });
    }
    {
        let cell = cell.clone();
        drop_target.connect_leave(move |_| {
            cell.remove_css_class("month-cell-drop-target");
        });
    }
    {
        let cell = cell.clone();
        let ctx = ctx.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            cell.remove_css_class("month-cell-drop-target");
            let Ok(event_id) = value.get::<i64>() else { return false };
            commit_month_drag(event_id, date, &ctx);
            true
        });
    }
    cell.add_controller(drop_target);
}

/// Shifts an event's start/end string (RFC 3339 for timed events, plain `YYYY-MM-DD`
/// for all-day ones — the same two representations `DisplayEvent`/`EventDetail` store,
/// per `DisplayEvent::start_date`'s doc comment) by `delta_days`, preserving
/// time-of-day/timezone for timed events. Shared by `commit_month_drag` for both
/// `start` and `end`, so a multi-day (or plain multi-hour) event keeps its duration —
/// no all-day-vs-timed branching needed at the call site.
fn shift_date_string(s: &str, delta_days: i64) -> Option<String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        Some((dt + Duration::days(delta_days)).to_rfc3339())
    } else {
        Some((NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()? + Duration::days(delta_days)).format("%Y-%m-%d").to_string())
    }
}

/// Persists a Month view drag-to-another-day for event `event_id`, following the same
/// look-up-detail → resolve-calendar/account → build-`EventEdits` → `update_event` →
/// `AppMsg::EventUpdated` sequence `commit_day_drag` uses — but shifting `start`/`end`
/// by the gap between the event's current start date and `target_date` (via
/// `shift_date_string`) rather than replacing them outright, so duration/time-of-day
/// (or an all-day event's span) carry over unchanged. A drop back onto the event's own
/// day is a no-op. Always ends by requesting a refresh, success or failure — matching
/// `commit_day_drag`'s own reasoning.
fn commit_month_drag(event_id: i64, target_date: NaiveDate, ctx: &EventCtx) {
    let detail = match event_detail(&ctx.storage, event_id) {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            tracing::warn!(event_id, "event no longer exists; discarding drag");
            ctx.sender.input(AppMsg::EventUpdated);
            return;
        }
        Err(err) => {
            tracing::warn!(%err, event_id, "failed to load event after drag");
            ctx.sender.input(AppMsg::EventUpdated);
            return;
        }
    };

    let Some(original_start_date) = NaiveDate::parse_from_str(detail.start.get(0..10).unwrap_or(&detail.start), "%Y-%m-%d").ok()
    else {
        tracing::warn!(event_id, "event start date failed to parse; discarding drag");
        ctx.sender.input(AppMsg::EventUpdated);
        return;
    };
    let delta_days = (target_date - original_start_date).num_days();
    if delta_days == 0 {
        return;
    }

    let (Some(new_start), Some(new_end)) = (shift_date_string(&detail.start, delta_days), shift_date_string(&detail.end, delta_days))
    else {
        tracing::warn!(event_id, "event start/end failed to parse; discarding drag");
        ctx.sender.input(AppMsg::EventUpdated);
        return;
    };

    let calendars = calendars_by_account(&ctx.storage).unwrap_or_default();
    let Some(calendar) = calendars.iter().find(|c| c.id == detail.calendar_id) else {
        tracing::warn!(event_id, "no calendar found for event; discarding drag");
        ctx.sender.input(AppMsg::EventUpdated);
        return;
    };

    let edits = EventEdits {
        title: detail.title,
        description: detail.description,
        location: detail.location,
        start: new_start,
        end: new_end,
        all_day: detail.all_day,
        color: detail.color,
        reminders: detail.reminders,
        busy: detail.busy,
        visibility: detail.visibility,
        recurrence: detail.recurrence,
    };
    if let Err(err) = update_event(&ctx.storage, event_id, detail.calendar_id, calendar.account_id, &edits) {
        tracing::warn!(%err, event_id, "failed to save dragged event");
    }
    ctx.sender.input(AppMsg::EventUpdated);
}

/// Pixel height of one hour row in the Day view's hour grid (`populate_day_hour_grid`)
/// — fixed, unlike the month grid's rows (`compute_max_visible_events`), since the Day
/// view is meant to scroll rather than shrink events to fit.
const DAY_ROW_HEIGHT_PX: i32 = 60;

/// *Minimum* width of the Day view's left-hand hour-label gutter (`populate_day_hour_grid`
/// column 0). Only a floor: each hour label gets this as its `width_request`, but a
/// label whose text plus `.day-hour-label` padding is wider (e.g. "11:00 PM" at a
/// larger system font) makes `GtkGrid` widen the whole column to its natural width. So
/// the gutter's *real* width is whatever the widest label measures, which
/// `populate_day_hour_grid` returns — and that measured value, never this constant, is
/// what the header row's timezone label, the all-day strip's gutter column, and the
/// event blocks/current-time line all align to. Using this constant directly for those
/// was exactly what left the all-day strip's dividers a few pixels left of the hour
/// grid's whenever the labels overflowed 52px.
const GUTTER_WIDTH_PX: i32 = 52;

/// The allowed values for `AppSettings::day_time_scale_minutes` (§12's Time scale
/// option), in zoomed-in-to-zoomed-out order — matches the increments Ctrl+scroll
/// cycles through on the Day view grid (`AppMsg::ZoomDayTimeScale`).
const DAY_TIME_SCALE_OPTIONS: [i64; 5] = [5, 10, 15, 30, 60];

/// Moves `current` one step through `DAY_TIME_SCALE_OPTIONS` — toward 5 (finer
/// gridlines, more zoomed in) when `finer` is true, toward 60 (coarser, more zoomed
/// out) otherwise. Clamps at either end rather than wrapping, and falls back to the
/// coarsest option if `current` isn't one of the five (shouldn't happen, since this is
/// the only place that writes the setting, but a stale/hand-edited value shouldn't panic).
fn cycle_day_time_scale(current: i64, finer: bool) -> i64 {
    let index = DAY_TIME_SCALE_OPTIONS.iter().position(|&v| v == current).unwrap_or(DAY_TIME_SCALE_OPTIONS.len() - 1);
    let next_index =
        if finer { index.saturating_sub(1) } else { (index + 1).min(DAY_TIME_SCALE_OPTIONS.len() - 1) };
    DAY_TIME_SCALE_OPTIONS[next_index]
}

/// The Day view header's corner label (`populate_day_header`) — the system time zone's
/// abbreviation (e.g. "EDT"), mirroring the Google Calendar PWA's day/week view corner
/// label. `chrono::Local` has no zone name of its own (just a numeric UTC offset, since
/// it doesn't know which IANA zone it came from), so this resolves the real zone via
/// `/etc/localtime`'s symlink target — reliable on the Arch/Omarchy target platform
/// (DESIGN_SPEC.md §3) — and formats through `chrono_tz`, which does carry
/// abbreviations. Falls back to the plain numeric offset if that symlink is missing or
/// unparseable, rather than guessing.
fn system_timezone_abbreviation(now: DateTime<Local>) -> String {
    let tz = std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| path.to_str().and_then(|s| s.split("zoneinfo/").nth(1).map(str::to_string)))
        .and_then(|name| name.parse::<Tz>().ok());

    match tz {
        Some(tz) => now.with_timezone(&tz).format("%Z").to_string(),
        None => now.format("%Z").to_string(),
    }
}

/// The header bar's view-switcher label for a given `ViewMode` — kept as a free function
/// (rather than a method on `ViewMode`) alongside `date_format_index`/`time_format_index`'s
/// similar UI-only mappings.
fn view_mode_label(view: ViewMode) -> &'static str {
    match view {
        ViewMode::Month => "Month",
        ViewMode::Day => "Day",
        ViewMode::FiveDay => "5 days",
        ViewMode::Custom => "Custom",
    }
}

/// One row of the header bar's view-switcher popover (`build_view_switcher_popover`):
/// either a live view, wired to `AppMsg::SetView` with its single-letter accelerator
/// (`install_view_shortcuts`), or a disabled "coming soon" placeholder for a view that
/// doesn't exist yet.
enum SwitcherRow {
    Live(ViewMode, &'static str),
    ComingSoon,
}

/// One row of the view-switcher popover's view-option checkbox group: a label, a
/// getter, and a setter for one `AppSettings` field — keeps `build_view_switcher_popover`
/// from needing three near-identical `gtk4::CheckButton` blocks.
type SettingsCheckboxSpec = (&'static str, fn(&AppSettings) -> bool, fn(&mut AppSettings, bool));

/// Builds the header bar's view-switcher popover (DESIGN_SPEC.md §10): an ordered list
/// of view rows — Schedule/Day/5 day/Week/Month/Year, matching the reference
/// screenshot's order — followed by a separator and the "Show weekends"/"Show declined
/// events"/"Show completed tasks" view-option checkboxes it also shows in the same
/// dropdown. Day, 5 day, and Month are live, wired to `AppMsg::SetView`; Schedule/Week/
/// Year stay disabled "coming soon" rows, matching the treatment
/// `calendar_customizer_button` gives "Settings and sharing", until those views exist
/// (§19's phased roadmap). Each live row also carries its single-letter accelerator in
/// a right-aligned `.shortcut-key` badge (`menu_row_button_with_hotkey`), so the hotkey
/// is discoverable straight from the menu rather than only from the `?` shortcuts
/// window. Built once in `init` (mirroring `calendar_customizer_button`'s imperative-
/// popover style) rather than declared in the `view!` macro, since popping the popover
/// down after a click needs a handle to it that the macro's own `connect_clicked =>
/// Msg` sugar doesn't give us.
///
/// "Show weekends" feeds `five_day_window` and "Show declined events" feeds
/// `filter_events` (both via `AppMsg::EventUpdated`'s next refresh) — real effects, not
/// no-ops. "Show completed tasks" is still a deliberate no-op settings write, matching
/// its app-wide scope everywhere else today (no Tasks service exists yet to supply
/// completion data — see its still-disabled/"Coming soon" Preferences-window row).
fn build_view_switcher_popover(sender: &ComponentSender<App>, storage: &Storage) -> gtk4::Popover {
    let popover = gtk4::Popover::new();

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    root.set_margin_all(6);
    root.set_width_request(200);

    for (label, row) in [
        ("Schedule", SwitcherRow::ComingSoon),
        ("Day", SwitcherRow::Live(ViewMode::Day, "D")),
        ("5 day", SwitcherRow::Live(ViewMode::FiveDay, "X")),
        ("Week", SwitcherRow::ComingSoon),
        ("Month", SwitcherRow::Live(ViewMode::Month, "M")),
        ("Year", SwitcherRow::ComingSoon),
    ] {
        match row {
            SwitcherRow::Live(view, key) => {
                let button = menu_row_button_with_hotkey(label, key);
                let sender = sender.clone();
                let popover_weak = popover.downgrade();
                button.connect_clicked(move |_| {
                    sender.input(AppMsg::SetView(view));
                    if let Some(popover) = popover_weak.upgrade() {
                        popover.popdown();
                    }
                });
                root.append(&button);
            }
            SwitcherRow::ComingSoon => {
                let disabled = gtk4::Label::new(Some(&format!("{label} (coming soon)")));
                disabled.set_halign(gtk4::Align::Start);
                disabled.set_sensitive(false);
                root.append(&disabled);
            }
        }
    }

    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let checkboxes: [SettingsCheckboxSpec; 3] = [
        ("Show weekends", |s| s.show_weekends, |s, v| s.show_weekends = v),
        ("Show declined events", |s| s.show_declined_events, |s, v| s.show_declined_events = v),
        ("Show completed tasks", |s| s.show_completed_tasks, |s, v| s.show_completed_tasks = v),
    ];
    for (label, get, set) in checkboxes {
        let checkbox = gtk4::CheckButton::builder().label(label).build();
        checkbox.set_active(get(&load_settings(storage).unwrap_or_default()));
        {
            let storage = storage.clone();
            let sender = sender.clone();
            checkbox.connect_toggled(move |cb| {
                let mut settings = load_settings(&storage).unwrap_or_default();
                set(&mut settings, cb.is_active());
                if let Err(err) = save_settings(&storage, &settings) {
                    tracing::warn!(%err, "failed to save preferences");
                }
                sender.input(AppMsg::EventUpdated);
            });
        }
        {
            // Re-syncs the checkbox's displayed state every time the popover opens —
            // it's built once in `init`, so without this a change made elsewhere (e.g.
            // the Preferences window's own "Show weekends" switch) would only show up
            // here after a restart.
            let storage = storage.clone();
            let checkbox_weak = checkbox.downgrade();
            popover.connect_show(move |_| {
                if let Some(checkbox) = checkbox_weak.upgrade() {
                    checkbox.set_active(get(&load_settings(&storage).unwrap_or_default()));
                }
            });
        }
        root.append(&checkbox);
    }

    popover.set_child(Some(&root));
    popover
}

/// Fills the Day/5-day view's header row (DESIGN_SPEC.md §10) for each date in `dates`
/// (1 date for `ViewMode::Day`, 5 for `ViewMode::FiveDay`): the primary timezone's
/// abbreviation (matching the width of the hour grid's gutter, so it lines up with the
/// hour labels below it, mirroring the Google Calendar PWA's day/week view corner
/// label) followed by one day cell per date — an uppercase weekday abbreviation over a
/// date number, the number badged in the accent color only when that date is `today`
/// (same convention as `populate_month_grid`'s `.today-badge`). With a single date, a
/// trailing gutter-width spacer balances the leading `tz_label` so the one day cell is
/// centered over the hour grid's event column rather than the whole header row — with
/// several dates this centering trick doesn't apply (each cell already evenly divides
/// the remaining width, matching `populate_day_hour_grid`'s per-day hour cells), so the
/// spacer is omitted. Also (re)builds the all-day strip directly underneath from any
/// `all_day` events overlapping `dates` — a column-homogeneous `gtk4::Grid` with one
/// column per date, indented by `gutter_px` (the width `populate_day_hour_grid`'s label
/// column actually measured, see `GUTTER_WIDTH_PX`), so a bar's edges land exactly
/// under the hour grid's day columns instead of drifting the way three
/// independently-laid-out containers could. Callers therefore run `populate_day_view`
/// first and pass the gutter width it returns.
/// `layout_all_day_events` computes each event's row/column span (clipped to `dates`,
/// with multi-day events spanning every column they cover via `Grid::attach`'s
/// `width` — a single widget rather than one per day, so the strip reads as one
/// continuous colored bar, `all_day_event_bar`), reusing `wire_event_click` so it opens
/// the same detail popover a month-view chip does. Hides itself via `set_visible` when
/// nothing overlaps `dates`, rather than always reserving empty space.
fn populate_day_header(
    header: &gtk4::Box,
    all_day: &gtk4::Grid,
    dates: &[NaiveDate],
    today: NaiveDate,
    events: &[DisplayEvent],
    ctx: &EventCtx,
    gutter_px: i32,
) {
    clear_children(header);

    let tz_label = gtk4::Label::new(Some(&system_timezone_abbreviation(Local::now())));
    tz_label.add_css_class("day-tz-label");
    tz_label.set_width_request(gutter_px);
    tz_label.set_halign(gtk4::Align::Start);
    tz_label.set_valign(gtk4::Align::End);
    header.append(&tz_label);

    for &date in dates {
        let day_cell = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        day_cell.set_hexpand(true);
        day_cell.set_halign(gtk4::Align::Center);
        day_cell.set_margin_top(4);
        day_cell.set_margin_bottom(4);
        let weekday_label = gtk4::Label::new(Some(&date.format("%a").to_string().to_uppercase()));
        weekday_label.add_css_class("day-header-weekday");
        if date == today {
            weekday_label.add_css_class("day-header-weekday-today");
        }
        day_cell.append(&weekday_label);

        let date_label = gtk4::Label::new(Some(&date.day().to_string()));
        date_label.add_css_class("day-header-date");
        if date == today {
            date_label.add_css_class("today-badge-lg");
        }
        day_cell.append(&date_label);

        header.append(&day_cell);
    }

    if dates.len() == 1 {
        // Balances the leading `tz_label` gutter so the single day cell is centered
        // over the hour grid's event column rather than the whole header row.
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_width_request(gutter_px);
        header.append(&spacer);
    }

    clear_children(all_day);
    all_day.set_column_spacing(0);
    // Homogeneous columns, one per date, so each gets exactly 1/N of the strip's width
    // no matter what's in it. A plain (non-homogeneous) `GtkGrid` grants every column
    // its content's *natural* width before sharing out the rest, so a column holding a
    // long-titled bar ("Company Holiday" plus its badge row) came out ~150px wider
    // than its empty neighbours and nothing lined up with the hour grid below. The
    // gutter can't be a column of a homogeneous grid (it'd be forced to the same width
    // as a day), so it's a start margin instead — `gutter_px`, the width the hour
    // grid's label column actually measured. Both grids then split the same remaining
    // width the same way (equal shares, leftover pixels to the leftmost columns), so
    // each all-day column boundary lands on the hour grid's matching divider.
    all_day.set_column_homogeneous(true);
    all_day.set_margin_start(gutter_px);
    all_day.set_hexpand(true);

    // Row 0 is a "pinning" row that's never used for event bars: a `GtkGrid` (even a
    // homogeneous one) skips columns no cell touches, so without this, a date with no
    // all-day events of its own (and no spanning bar crossing it) would collapse to
    // zero width instead of taking its equal share.
    for day_index in 0..dates.len() {
        let day_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        day_spacer.set_hexpand(true);
        day_spacer.set_size_request(-1, 0);
        all_day.attach(&day_spacer, day_index as i32, 0, 1, 1);
    }

    let layout = layout_all_day_events(events, dates);
    // Column dividers between adjacent dates, each spanning every row (the pinning
    // row plus all bar rows) so the line runs the strip's full height and reads as a
    // continuation of the hour grid's `.day-hour-cell-divider` below. Attached before
    // the bars so a bar that crosses a boundary draws over the line, not under it.
    let row_count = 1 + layout.iter().map(|item| item.row + 1).max().unwrap_or(0);
    for day_index in 0..dates.len().saturating_sub(1) {
        let divider = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        divider.add_css_class("day-all-day-column-divider");
        divider.set_valign(gtk4::Align::Fill);
        all_day.attach(&divider, day_index as i32, 0, 1, row_count as i32);
    }
    for item in &layout {
        let bar = all_day_event_bar(item.event, item.clipped_start, item.clipped_end);
        wire_event_click(&bar, item.event, ctx);
        all_day.attach(&bar, item.start_col as i32, 1 + item.row as i32, item.span_cols as i32, 1);
    }
    all_day.set_visible(!layout.is_empty());
}

/// Which "boundary" a Day view hour-grid row's `minute_of_day % 60` lands on —
/// `populate_day_hour_grid`'s label text/CSS class and gridline weight both key off
/// this, so a row's line and its label always agree on how significant it is.
/// `QuarterHour`/`Minor` only actually occur at finer `DAY_TIME_SCALE_OPTIONS` values
/// (15 and 5 minutes respectively divide evenly into quarter-hours; 10 minutes doesn't,
/// so its non-hour/half-hour rows land in `Minor` instead) — every option produces
/// exactly the tiers it can, with no per-scale special-casing needed here.
enum DayHourMarkTier {
    Hour,
    HalfHour,
    QuarterHour,
    Minor,
}

fn day_hour_mark_tier(minute_of_hour: i64) -> DayHourMarkTier {
    match minute_of_hour {
        0 => DayHourMarkTier::Hour,
        30 => DayHourMarkTier::HalfHour,
        15 | 45 => DayHourMarkTier::QuarterHour,
        _ => DayHourMarkTier::Minor,
    }
}

/// Fills the Day/5-day view's hour grid (DESIGN_SPEC.md §12's Time scale option) with
/// one fixed-height row per `scale_minutes` interval (24*60/`scale_minutes` rows —
/// always a whole number, since `DAY_TIME_SCALE_OPTIONS` all divide 60 evenly): a time
/// label in the left gutter at every hour/half-hour/quarter-hour boundary (blank at any
/// finer residual tick a 5- or 10-minute scale adds, and blank at midnight — matching
/// Google Calendar's own day view, which doesn't label the very top edge), each styled
/// per `DayHourMarkTier` so hour labels read as most significant, then half-hour, then
/// quarter-hour. `day_count` cells to its right (one per day column; `1` for
/// `ViewMode::Day`, `5` for `ViewMode::FiveDay`) draw that row's horizontal line, its
/// weight following the same tier (heaviest on the hour, lightest for a finer residual tick). Each
/// row is `DAY_ROW_HEIGHT_PX` tall regardless of the grid's allocated size, since
/// (unlike `populate_month_grid`'s row-homogeneous stretch-to-fit grid) the Day view is
/// meant to scroll, not shrink events to fit — so a finer `scale_minutes` (more, shorter
/// intervals) makes the whole grid taller rather than each row shorter. Called on every
/// `App::refresh`.
///
/// Returns the label column's resulting width in pixels: the widest label's natural
/// width (measured right after attaching it, so its `.day-hour-label` CSS padding is
/// already in — GTK resolves a fresh node's style lazily on first measure), never
/// less than `GUTTER_WIDTH_PX`. That's exactly the width `GtkGrid` will allocate
/// column 0 (a non-expanding column gets its natural width before any leftover goes to
/// the hexpand day columns), so everything that has to line up with the day columns —
/// the header/all-day strip via `populate_day_header`, the overlay children via
/// `day_column_geometries` — keys off this value rather than the constant.
fn populate_day_hour_grid(grid: &gtk4::Grid, time_format: TimeFormat, scale_minutes: i64, day_count: usize) -> i32 {
    clear_children(grid);
    grid.set_column_spacing(0);

    let mut gutter_px = GUTTER_WIDTH_PX;
    let row_count = 24 * 60 / scale_minutes;
    for row in 0..row_count {
        let minute_of_day = row * scale_minutes;
        let minute_of_hour = minute_of_day % 60;
        let tier = day_hour_mark_tier(minute_of_hour);

        let label_text = if minute_of_day == 0 {
            String::new()
        } else {
            match tier {
                DayHourMarkTier::Minor => String::new(),
                DayHourMarkTier::Hour | DayHourMarkTier::HalfHour | DayHourMarkTier::QuarterHour => {
                    let hour = (minute_of_day / 60) as u32;
                    format_clock(NaiveTime::from_hms_opt(hour, minute_of_hour as u32, 0).expect("valid time"), time_format)
                }
            }
        };
        let label = gtk4::Label::new(Some(&label_text));
        label.add_css_class("day-hour-label");
        match tier {
            DayHourMarkTier::Hour | DayHourMarkTier::Minor => {}
            DayHourMarkTier::HalfHour => label.add_css_class("day-hour-label-half"),
            DayHourMarkTier::QuarterHour => label.add_css_class("day-hour-label-quarter"),
        }
        label.set_halign(gtk4::Align::End);
        label.set_valign(gtk4::Align::Start);
        label.set_width_request(GUTTER_WIDTH_PX);
        grid.attach(&label, 0, row as i32, 1, 1);
        let (_, natural_width, _, _) = label.measure(gtk4::Orientation::Horizontal, -1);
        gutter_px = gutter_px.max(natural_width);

        for day_index in 0..day_count {
            let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            cell.add_css_class("day-hour-cell");
            if row == 0 {
                cell.add_css_class("day-hour-cell-first");
            } else {
                match tier {
                    DayHourMarkTier::Hour => {}
                    DayHourMarkTier::HalfHour => cell.add_css_class("day-hour-cell-half"),
                    DayHourMarkTier::QuarterHour => cell.add_css_class("day-hour-cell-quarter"),
                    DayHourMarkTier::Minor => cell.add_css_class("day-hour-cell-minor"),
                }
            }
            if day_count > 1 && day_index + 1 < day_count {
                cell.add_css_class("day-hour-cell-divider");
            }
            cell.set_hexpand(true);
            cell.set_size_request(-1, DAY_ROW_HEIGHT_PX);
            grid.attach(&cell, 1 + day_index as i32, row as i32, 1, 1);
        }
    }
    gutter_px
}

/// Removes every overlay child `populate_day_view` previously added on top of the
/// hour grid (the current-time line/dot, event blocks) without touching the grid
/// itself — the grid is the Overlay's main child (`gtk4::Overlay::set_child`, set once
/// via the `day-hour-grid` CSS class tag rather than by identity, since a plain
/// pointer/`PartialEq` check reads less clearly here) and must survive so
/// `populate_day_hour_grid` can keep repopulating it in place. Mirrors the
/// clear-then-repopulate convention `clear_children` gives every other view
/// (`populate_month_grid`, `populate_sidebar`, ...), just scoped to `Overlay`'s split
/// main-child/overlay-child model instead of a plain container's flat child list.
fn clear_day_overlay_extras(overlay: &gtk4::Overlay) {
    let mut child = overlay.first_child();
    while let Some(widget) = child {
        let next = widget.next_sibling();
        if !widget.has_css_class("day-hour-grid") {
            overlay.remove_overlay(&widget);
        }
        child = next;
    }
}

/// Rebuilds the Day/5-day view's scrollable body for `dates` (1 date for
/// `ViewMode::Day`, 5 for `ViewMode::FiveDay`): the hour grid
/// (`populate_day_hour_grid`), every non-all-day event on each date positioned by time
/// within that date's own column (`day_event_block`), and — only for whichever date
/// equals `today`, if any is currently shown — a red current-time line and dot
/// (mirroring Google Calendar's own day view) at the current local time's vertical
/// offset, confined to that date's column. Safe to call on every `App::refresh`, like
/// every other `populate_*` function in this file. With `dates.len() == 1` this
/// computes the exact same single day-column geometry as before generalizing to N
/// days. Returns the hour grid's measured gutter width (see `populate_day_hour_grid`)
/// so the caller can hand it to `populate_day_header`.
fn populate_day_view(
    overlay: &gtk4::Overlay,
    dates: &[NaiveDate],
    today: NaiveDate,
    events: &[DisplayEvent],
    ctx: &EventCtx,
    scale_minutes: i64,
    ctrl_snap_minutes: i64,
    ctrl_shift_snap_minutes: i64,
    hold_ms: i64,
) -> i32 {
    // The grid itself is the Overlay's persistent main child (declared in the `view!`
    // macro), so only its own children get cleared/rebuilt here — `Overlay`'s other
    // children (the previous call's now-line/dot/event blocks) go through
    // `clear_day_overlay_extras` instead.
    let gutter_px = match overlay.child().and_downcast::<gtk4::Grid>() {
        Some(grid) => populate_day_hour_grid(&grid, ctx.time_format, scale_minutes, dates.len()),
        None => GUTTER_WIDTH_PX,
    };
    clear_day_overlay_extras(overlay);

    let pixels_per_minute = DAY_ROW_HEIGHT_PX as f64 / scale_minutes as f64;
    // `overlay.width()` reads 0 before the window's first real layout pass (same
    // Wayland/compositor-round-trip issue `compute_max_visible_events` documents for
    // the month grid's height) — `init`'s size-polling timer and the `default-width`
    // resize watcher both re-run this function once a real width is available, so a
    // brief undersized first frame self-corrects rather than needing special-casing
    // here.
    let columns = day_column_geometries(overlay.width(), gutter_px, dates.len());

    for (day_index, (&date, &column)) in dates.iter().zip(&columns).enumerate() {
        let date_key = date.format("%Y-%m-%d").to_string();
        for layout in layout_day_events(events, &date_key) {
            day_event_block(
                overlay,
                layout.event,
                layout.lane,
                layout.columns,
                column,
                pixels_per_minute,
                scale_minutes,
                ctrl_snap_minutes,
                ctrl_shift_snap_minutes,
                hold_ms,
                dates,
                day_index,
                ctx,
            );
        }

        if date == today {
            let now = Local::now();
            let now_px = ((now.hour() as f64 * 60.0 + now.minute() as f64) * pixels_per_minute).round() as i32;

            let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            line.add_css_class("day-now-line");
            line.set_valign(gtk4::Align::Start);
            line.set_margin_top(now_px);
            line.set_margin_start(column.x_offset_px);
            if dates.len() == 1 {
                line.set_halign(gtk4::Align::Fill);
            } else {
                line.set_halign(gtk4::Align::Start);
                line.set_size_request(column.width_px, -1);
            }
            overlay.add_overlay(&line);

            let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            dot.add_css_class("day-now-dot");
            dot.set_valign(gtk4::Align::Start);
            dot.set_halign(gtk4::Align::Start);
            dot.set_margin_top(now_px - 4);
            dot.set_margin_start(column.x_offset_px - 4);
            overlay.add_overlay(&dot);
        }
    }
    gutter_px
}

/// The most columns overlapping events on the same day split into — beyond this,
/// additional simultaneous events just share the narrowest (sixth) column rather than
/// subdividing further.
const MAX_DAY_EVENT_COLUMNS: usize = 6;

/// One event positioned within a day's overlap layout: `lane` is its column index
/// (0-based) and `columns` is how many columns its overlap cluster was split into —
/// e.g. two events double-booked into the same half-hour get `columns: 2` and lanes
/// `0`/`1`, so `day_event_block` can give each exactly half the day column's width.
struct DayEventLayout<'a> {
    event: &'a DisplayEvent,
    lane: usize,
    columns: usize,
}

/// Lays out every non-all-day event on `date` into side-by-side columns: a greedy
/// interval-sweep (events sorted by start time; each takes the lowest column index not
/// already occupied by a still-open event) assigns lanes, and each maximal run of
/// mutually-overlapping events (a "cluster" — the active set never goes empty within
/// one) is then stamped with `columns` = the most columns simultaneously open at any
/// point in that cluster, capped at `MAX_DAY_EVENT_COLUMNS`. Two overlapping events
/// split the day column in half, three in thirds, and so on up to sixths — beyond six
/// simultaneous events, the extras share the last column rather than subdividing
/// further.
fn layout_day_events<'a>(events: &'a [DisplayEvent], date_key: &str) -> Vec<DayEventLayout<'a>> {
    let mut items: Vec<(&DisplayEvent, f64, f64)> = events
        .iter()
        .filter(|e| !e.all_day && e.start_date() == date_key)
        .filter_map(|e| {
            let start = DateTime::parse_from_rfc3339(&e.start).ok()?.with_timezone(&Local);
            let end = DateTime::parse_from_rfc3339(&e.end).ok()?.with_timezone(&Local);
            let start_min = start.hour() as f64 * 60.0 + start.minute() as f64;
            let duration_min = (end - start).num_minutes().max(15) as f64;
            Some((e, start_min, start_min + duration_min))
        })
        .collect();
    items.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // Stamps every item in `out[cluster_start..]` with its cluster's final column
    // count, capped at `MAX_DAY_EVENT_COLUMNS` — and, since the greedy sweep below can
    // hand out lanes past that cap when more than `MAX_DAY_EVENT_COLUMNS` events truly
    // overlap, also clamps each item's `lane` back into `0..columns` so the two fields
    // stay a valid pair (`day_event_block` divides width by `columns` and indexes by
    // `lane`, so an uncapped lane there would compute a negative/out-of-range offset).
    // The extras this clamp collapses onto the last column is the documented "beyond
    // six, they share the narrowest column" behavior.
    fn close_cluster(out: &mut [DayEventLayout], columns: usize) {
        for item in out {
            item.columns = columns;
            item.lane = item.lane.min(columns - 1);
        }
    }

    let mut active: Vec<(f64, usize)> = Vec::new(); // (end_min, lane), still-open events
    let mut out: Vec<DayEventLayout> = Vec::with_capacity(items.len());
    let mut cluster_start = 0usize; // index into `out` where the current cluster began
    let mut cluster_peak = 0usize; // most columns simultaneously open in this cluster

    for (event, start_min, end_min) in items {
        active.retain(|&(active_end, _)| active_end > start_min);
        if active.is_empty() && cluster_start < out.len() {
            close_cluster(&mut out[cluster_start..], cluster_peak.min(MAX_DAY_EVENT_COLUMNS));
            cluster_start = out.len();
            cluster_peak = 0;
        }

        let used: HashSet<usize> = active.iter().map(|&(_, lane)| lane).collect();
        let mut lane = 0;
        while used.contains(&lane) {
            lane += 1;
        }
        active.push((end_min, lane));
        cluster_peak = cluster_peak.max(active.len());

        out.push(DayEventLayout { event, lane, columns: 1 });
    }
    if cluster_start < out.len() {
        close_cluster(&mut out[cluster_start..], cluster_peak.min(MAX_DAY_EVENT_COLUMNS));
    }

    out
}

/// One all-day event's position within `populate_day_header`'s all-day strip: `row` is
/// its vertical slot (0-based, shared across every date-column it touches, so two
/// events assigned the same row never overlap in date range) and `start_col`/
/// `span_cols` mark which of `dates`'s columns it covers — both already clipped to
/// `dates`'s own bounds, so `populate_day_header` can hand them straight to
/// `Grid::attach` without any further date arithmetic. `clipped_start`/`clipped_end`
/// record whether the event's *real* (unclipped) range extends past `dates[0]`/
/// `dates.last()` respectively, so `all_day_event_bar` knows which edge(s), if any,
/// need a continuation chevron instead of a plain rounded corner.
struct AllDayEventLayout<'a> {
    event: &'a DisplayEvent,
    row: usize,
    start_col: usize,
    span_cols: usize,
    clipped_start: bool,
    clipped_end: bool,
}

/// Lays out every `all_day` event overlapping `dates` into rows: a greedy sweep (events
/// sorted by clipped start date, then by span length descending, then by title for a
/// deterministic tie-break) hands each event the lowest-numbered row whose
/// last-occupied date is before this event's own clipped start — the same
/// greedy-interval idea `layout_day_events` uses for its lanes, just measured in whole
/// days instead of minutes, and with no analog to `MAX_DAY_EVENT_COLUMNS`: an
/// unbounded number of simultaneous multi-day events just makes the strip taller, not
/// narrower, so there's no reason to cap rows and start collapsing them together.
/// Longer events sort first within a shared start date so a week-long event claims row
/// 0 ahead of a same-day one-off, mirroring Google Calendar's own banner stacking.
fn layout_all_day_events<'a>(events: &'a [DisplayEvent], dates: &[NaiveDate]) -> Vec<AllDayEventLayout<'a>> {
    let Some(&window_start) = dates.first() else { return Vec::new() };
    let window_end = *dates.last().expect("dates is non-empty (checked via .first() above)");

    let mut items: Vec<(&DisplayEvent, NaiveDate, NaiveDate, bool, bool)> = events
        .iter()
        .filter(|e| e.all_day)
        .filter_map(|e| {
            let start = NaiveDate::parse_from_str(e.start_date(), "%Y-%m-%d").ok()?;
            let end_exclusive = NaiveDate::parse_from_str(e.end.get(0..10).unwrap_or(&e.end), "%Y-%m-%d").ok()?;
            let end = end_exclusive.pred_opt().unwrap_or(end_exclusive).max(start);
            if end < window_start || start > window_end {
                return None;
            }
            let clipped_start = start.max(window_start);
            let clipped_end = end.min(window_end);
            Some((e, clipped_start, clipped_end, start < window_start, end > window_end))
        })
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1).then((b.2 - b.1).cmp(&(a.2 - a.1))).then(a.0.title.cmp(&b.0.title)));

    let mut row_occupied_through: Vec<NaiveDate> = Vec::new();
    let mut out = Vec::with_capacity(items.len());
    for (event, clipped_start, clipped_end, clipped_start_flag, clipped_end_flag) in items {
        let row = match row_occupied_through.iter().position(|&occupied_through| occupied_through < clipped_start) {
            Some(row) => {
                row_occupied_through[row] = clipped_end;
                row
            }
            None => {
                row_occupied_through.push(clipped_end);
                row_occupied_through.len() - 1
            }
        };
        out.push(AllDayEventLayout {
            event,
            row,
            start_col: (clipped_start - window_start).num_days() as usize,
            span_cols: (clipped_end - clipped_start).num_days() as usize + 1,
            clipped_start: clipped_start_flag,
            clipped_end: clipped_end_flag,
        });
    }
    out
}

/// One day's horizontal slot within the Day/5-day view's hour grid — `width_px` is
/// that day's share of the grid's total width (the whole width for `ViewMode::Day`,
/// about one-fifth of it for `ViewMode::FiveDay`), `x_offset_px` is where that slot
/// starts (the measured gutter width for the single Day-view column, or that plus the
/// preceding columns' widths for a 5-day column — see `day_column_geometries`).
/// Bundled into one struct, rather than two more `day_event_block` parameters, to keep
/// its argument count down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DayColumnGeometry {
    width_px: i32,
    x_offset_px: i32,
}

/// Splits the hour grid's allocated `overlay_width` into one `DayColumnGeometry` per
/// day, reproducing exactly how `GtkGrid` lays out `populate_day_hour_grid`'s columns:
/// column 0 takes `gutter_px` (its natural width, which a non-expanding column is
/// granted before any leftover is shared out), and what remains is divided among the
/// `day_count` hexpand day columns — with the `remaining % day_count` pixels the
/// integer division can't split going one each to the *leftmost* columns, which is the
/// order `GtkGrid` hands out its remainder in. Reproducing that split, rather than a
/// uniform `total / day_count` (or the previous version's guessed `- 8` for a scrollbar
/// this overlay-scrolling `ScrolledWindow` never reserves), keeps every
/// absolutely-positioned overlay child — event blocks, the current-time line and dot —
/// on the same pixel as the gridline it belongs to, all the way to the last column.
/// Floors the day columns at 60px each so a not-yet-laid-out first frame
/// (`overlay_width == 0`, see `populate_day_view`) still yields usable geometry.
fn day_column_geometries(overlay_width: i32, gutter_px: i32, day_count: usize) -> Vec<DayColumnGeometry> {
    let day_count = day_count.max(1);
    let total = (overlay_width - gutter_px).max(60 * day_count as i32);
    let base = total / day_count as i32;
    let remainder = (total % day_count as i32) as usize;
    let mut x_offset_px = gutter_px;
    (0..day_count)
        .map(|index| {
            let width_px = base + i32::from(index < remainder);
            let column = DayColumnGeometry { width_px, x_offset_px };
            x_offset_px += width_px;
            column
        })
        .collect()
}

/// Threshold `day_event_block` collapses its time bubble to start-only at: below this
/// many characters' worth of leftover subject width, the full "start–end" bubble is
/// judged to be crowding the subject out rather than sharing the card with it.
const MIN_SUBJECT_VISIBLE_CHARS: f64 = 20.0;

/// The pixel width `day_event_block`'s `text_column` would have left for `subject` once
/// `card`'s fixed chrome (its own `Box` spacing, `text_column`'s right margin, and
/// `time_bubble`'s own natural width plus its left margin) is subtracted from
/// `card_width` — the same arithmetic `card`'s `gtk4::Box` layout performs internally,
/// just computed ahead of time so the decision of *which* bubble text to build can be
/// made before either widget exists. `char_width_px` is `subject`'s own font's
/// Pango-measured `approximate_char_width`, i.e. "one character" in whatever font size
/// `.day-event-subject`'s CSS actually resolves to, not a hardcoded guess.
fn subject_has_room(card_width: i32, time_bubble_natural_width: i32, char_width_px: f64) -> bool {
    // Mirrors `card`'s own `gtk4::Box::new(Horizontal, 4)` spacing, `time_bubble`'s
    // `set_margin_start(4)`, and `text_column`'s `set_margin_end(6)` — kept as named
    // constants here rather than imported from `day_event_block` since GTK doesn't
    // expose a "how much space would this box give child X" query to compute it from
    // the widgets themselves ahead of layout.
    const CARD_SPACING_PX: i32 = 4;
    const TEXT_COLUMN_MARGIN_END_PX: i32 = 6;
    const TIME_BUBBLE_MARGIN_START_PX: i32 = 4;
    let available = card_width - CARD_SPACING_PX - TEXT_COLUMN_MARGIN_END_PX - TIME_BUBBLE_MARGIN_START_PX - time_bubble_natural_width;
    available as f64 >= char_width_px * MIN_SUBJECT_VISIBLE_CHARS
}

/// One positioned event block for the Day/5-day view's hour grid: top offset and
/// height come from the event's start time and duration (in the viewer's local time —
/// Google Calendar events are stored/queried in UTC/RFC 3339, per `DisplayEvent`),
/// scaled by `pixels_per_minute` (which follows the current Time scale setting, §12).
/// `lane`/`columns` (from `layout_day_events`) split `column.width_px` evenly so
/// overlapping events share the day column side by side instead of drawing on top of
/// each other; `column.x_offset_px` positions the whole block in the right day column
/// to begin with (see `DayColumnGeometry`).
///
/// Internally a horizontal split: a start–end time bubble pinned to the left at its
/// own natural size, and a subject/body text column to its right with `hexpand: true`
/// that gets whatever width is left over — ordinary `GtkBox` allocation, not manual
/// math, so the text column shrinks to make room for the bubble instead of either one
/// fighting the other for space.
///
/// Getting the subject/body labels to actually *use* that leftover width takes two
/// cooperating pieces, both required — dropping either one reintroduces a real bug this
/// function shipped with once already:
/// - `max_width_chars(1)` caps their *natural* (preferred) width to ~1 character. This
///   is for `card`'s sake, not the labels': `card` is a non-`Fill`, absolutely-positioned
///   `Overlay` child (`GtkOverlay` allocates such children at `clamp(natural, minimum,
///   overlay_width)`, honoring *natural* size, not just `size_request`'s minimum), so
///   without this cap a long title's full natural width would propagate up through
///   `text_column` into `card`'s own natural size and blow `card` past its intended
///   `column_width` — see `layout_day_events`'s doc comment for why `column_width`
///   itself is trustworthy.
/// - `halign(Fill)` (plus `xalign(0.0)` so the *drawn text* still starts at the left
///   edge — `xalign` is `GtkLabel`'s own text-position property, independent of the
///   widget-level `halign`) is for the labels' own sake. GTK4 box allocation has two
///   regimes: when a child's natural size *exceeds* the space available, it's squeezed
///   toward its minimum regardless of `halign` (this is how `event_row`, elsewhere in
///   this file, ellipsizes correctly with no `max_width_chars` at all — it's always in
///   this regime, since its ancestors are a plain constrained `Grid`/`Box`, not an
///   `Overlay`). But once `max_width_chars(1)` above makes a label's natural width
///   *smaller* than the cell `text_column`'s `hexpand` earns it, the label flips into
///   the opposite regime — extra room to grow into — and `halign` alone decides whether
///   it takes that room (`Fill`) or just sits at its tiny natural size leaving the rest
///   of the cell blank (`Start`/`Center`/`End`). Without `Fill` here, that's exactly
///   what happened: every event rendered as a bare "…" with a wide empty gap before the
///   time bubble. This `Fill` + `xalign` + `max_width_chars` + `ellipsize` combination
///   has no other precedent in this file — `show_event_popover`'s labels pair `Start` +
///   `xalign(0.0)` + `wrap(true)` instead, which suits their genuinely-constrained
///   ancestor chain but would not fix this Overlay-rooted one.
///
/// `time_bubble` still has no `max_width_chars`/`ellipsize` of its own, so at a high
/// overlap count its natural width could in principle still exceed the lane's
/// `column_width` even after collapsing to the start-only text below — narrower, but
/// not unboundedly so. Collapsing to start-only (`subject_has_room`) covers the common
/// case; a full fix would still need truncating the time text itself or reworking how
/// `card`'s width is pinned.
///
/// Wired via `install_day_event_drag`, not `wire_event_click`: a plain click (no
/// pointer movement) still opens the same `show_event_popover` detail card a
/// month-view chip does, but a real drag on the body/top edge/bottom edge instead
/// moves/resizes the event. Adds nothing (and returns early) if `start`/`end` don't
/// parse as RFC 3339 (shouldn't happen for a non-all-day `DisplayEvent`, but
/// `populate_day_view` iterates a whole day's events and one bad row shouldn't blank
/// the rest).
///
/// Builds *two* overlapping widgets, not one — `hit_box` (invisible, owns the drag
/// gesture) and `card` (visible, purely decorative for input) — rather than one card
/// that's both clicked/dragged and moved: see `install_day_event_drag`'s doc comment
/// for why the widget a `GestureDrag` reads its motion offsets from can never also be
/// the widget that drag repositions. Both are added directly to `overlay` here (rather
/// than returned for the caller to add), since there are now two.
fn day_event_block(
    overlay: &gtk4::Overlay,
    event: &DisplayEvent,
    lane: usize,
    columns: usize,
    column: DayColumnGeometry,
    pixels_per_minute: f64,
    scale_minutes: i64,
    ctrl_snap_minutes: i64,
    ctrl_shift_snap_minutes: i64,
    hold_ms: i64,
    dates: &[NaiveDate],
    day_index: usize,
    ctx: &EventCtx,
) {
    let Some(start) = DateTime::parse_from_rfc3339(&event.start).ok().map(|d| d.with_timezone(&Local)) else { return };
    let Some(end) = DateTime::parse_from_rfc3339(&event.end).ok().map(|d| d.with_timezone(&Local)) else { return };

    let start_minutes = start.hour() as f64 * 60.0 + start.minute() as f64;
    let duration_minutes = (end - start).num_minutes().max(15) as f64;
    let top = (start_minutes * pixels_per_minute).round() as i32;
    let height = (duration_minutes * pixels_per_minute).round() as i32;

    let column_width = column.width_px / columns as i32;
    const COLUMN_GAP_PX: i32 = 2;

    let card = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    card.add_css_class("day-event-block");
    if event.self_response_status == Some(AttendeeResponseStatus::Declined) {
        card.add_css_class("event-declined");
    }
    if let Some(color) = &event.color {
        card.add_css_class(&css_class_for_color(color));
    }
    card.set_valign(gtk4::Align::Start);
    card.set_halign(gtk4::Align::Start);
    // `card`'s own hexpand defaults to "auto" (unset), which would otherwise inherit
    // `true` from the text column below once that's hexpand — pinning it here keeps
    // the card's width authoritatively at `set_size_request` below regardless.
    card.set_hexpand(false);
    card.set_overflow(gtk4::Overflow::Hidden);
    let card_width = (column_width - COLUMN_GAP_PX).max(20);
    let card_margin_start = column.x_offset_px + 4 + lane as i32 * column_width;
    card.set_margin_top(top);
    card.set_margin_start(card_margin_start);
    card.set_size_request(card_width, height.max(18));
    // Excluded from input picking entirely: `hit_box` (below) owns the drag gesture
    // instead, so pointer events pass straight through `card` to it.
    card.set_can_target(false);

    let text_column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    text_column.set_hexpand(true);
    // Explicit even though `Fill` is GTK4's own default here — stating it removes the
    // exact "relying on an unstated default one level up" gap that let `subject`/`body`
    // below ship with the wrong `halign` in the first place (see their doc comment).
    text_column.set_halign(gtk4::Align::Fill);
    text_column.set_valign(gtk4::Align::Start);
    // On the right, not the left — `time_bubble` (appended before this, below) now sits
    // to this column's left, so its edge-breathing-room margin moved with it.
    text_column.set_margin_end(6);
    text_column.set_margin_top(3);
    text_column.set_margin_bottom(3);

    let subject = gtk4::Label::new(Some(&event.title));
    subject.add_css_class("day-event-subject");
    subject.set_halign(gtk4::Align::Fill);
    subject.set_xalign(0.0);
    subject.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    subject.set_max_width_chars(1);
    subject.set_hexpand(true);
    text_column.append(&subject);

    if let Some(description) = event.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        // First line only — the block has no room for a real multi-line body, and a
        // one-line preview ellipsized the same way as the subject is what "as much
        // body text as fits" comes down to at these heights.
        let body = gtk4::Label::new(description.lines().next());
        body.add_css_class("day-event-body");
        body.set_halign(gtk4::Align::Fill);
        body.set_xalign(0.0);
        body.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        body.set_max_width_chars(1);
        body.set_hexpand(true);
        text_column.append(&body);
    }

    // Its own line, after the subject/body text, rather than inline alongside
    // `subject` — `subject_has_room`'s width budget above only accounts for
    // `time_bubble`'s width against `card_width`, so folding badges into that same
    // row would silently invalidate that calculation. A card too short to fit this
    // (a very brief event) just clips it via `card`'s `Overflow::Hidden`, the same
    // degradation a too-long `body` line already gets.
    if let Some(badges) = event_badge_row(event) {
        text_column.append(&badges);
    }

    let time_bubble = gtk4::Label::new(None);
    time_bubble.add_css_class("day-event-time-bubble");
    // Left-aligned, and appended to `card` before `text_column` below — the bubble sits
    // at the card's left edge, with the subject/body text column filling the rest of
    // the width to its right (mirrored from the original right-aligned layout).
    time_bubble.set_halign(gtk4::Align::Start);
    time_bubble.set_valign(gtk4::Align::Start);
    time_bubble.set_margin_top(3);
    time_bubble.set_margin_start(4);

    // Built and measured with the full range first — `subject_has_room` needs the
    // bubble's real, CSS-styled natural width (pill padding included) to know whether
    // the subject line would be left with room for `MIN_SUBJECT_VISIBLE_CHARS` or more;
    // "chars worth" is Pango's own `approximate_char_width` metric (what `GtkLabel`'s
    // `max-width-chars`/`width-chars` are themselves built on) for `subject`'s actual
    // font, not a hand-guessed pixels-per-char constant. Collapsing to start-only here,
    // before either widget is added to `card`, means the card is sized/painted right on
    // its first frame instead of jumping after an initial full-range layout.
    let full_range = format!("{}–{}", format_clock(start.time(), ctx.time_format), format_clock(end.time(), ctx.time_format));
    time_bubble.set_label(&full_range);
    let time_bubble_width = time_bubble.measure(gtk4::Orientation::Horizontal, -1).1;
    let char_width_px = subject.pango_context().metrics(None, None).approximate_char_width() as f64 / gtk4::pango::SCALE as f64;
    let start_only = !subject_has_room(card_width, time_bubble_width, char_width_px);
    if start_only {
        time_bubble.set_label(&format_clock(start.time(), ctx.time_format));
    }
    card.append(&time_bubble);
    card.append(&text_column);

    // Invisible, and never moved again once created — see `install_day_event_drag`'s
    // doc comment for why this stability matters. Same initial geometry as `card`, so
    // the zone classification/hover cursor it drives lines up with what's on screen.
    let hit_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    hit_box.set_valign(gtk4::Align::Start);
    hit_box.set_halign(gtk4::Align::Start);
    hit_box.set_margin_top(top);
    hit_box.set_margin_start(card_margin_start);
    hit_box.set_size_request(card_width, height.max(18));
    hit_box.set_cursor_from_name(Some("pointer"));

    // Hidden until a drag actually starts (`install_day_event_drag`'s `connect_drag_begin`
    // shows it, `connect_drag_end` hides it again) — its text is fixed at build time
    // (today's configured snap intervals don't change mid-drag), so only its visibility
    // and vertical position (tracking the card's current top, to stay just above it as
    // it moves) need to change during the gesture.
    let drag_hint = gtk4::Label::new(Some(&format!(
        "Ctrl: {ctrl_snap_minutes}m snap   ·   Ctrl+Shift: {ctrl_shift_snap_minutes}m snap"
    )));
    drag_hint.add_css_class("day-drag-hint");
    drag_hint.set_halign(gtk4::Align::Start);
    drag_hint.set_valign(gtk4::Align::Start);
    drag_hint.set_margin_start(card_margin_start);
    drag_hint.set_margin_top((top - DAY_DRAG_HINT_OFFSET_PX).max(0));
    drag_hint.set_visible(false);

    install_day_event_drag(
        &hit_box,
        &card,
        &time_bubble,
        &drag_hint,
        event,
        card_width,
        card_margin_start,
        pixels_per_minute,
        scale_minutes,
        ctrl_snap_minutes,
        ctrl_shift_snap_minutes,
        hold_ms,
        start_only,
        dates.to_vec(),
        day_index,
        column.width_px,
        ctx,
    );

    overlay.add_overlay(&hit_box);
    overlay.add_overlay(&card);
    overlay.add_overlay(&drag_hint);
}

/// Which part of a Day-view event card a drag grabbed, decided once in
/// `connect_drag_begin` from the press's local y vs. the card's allocated height
/// (`classify_day_drag_zone`) and held fixed for that gesture — mirrors Google
/// Calendar's day-view affordances (drag the body to move, either edge to resize).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DayDragZone {
    Move,
    ResizeTop,
    ResizeBottom,
}

/// Per-gesture state `install_day_event_drag`'s three `GestureDrag` callbacks share:
/// captured once in `connect_drag_begin` (`zone`, and the pre-drag start/end every
/// later delta is computed relative to, since the offsets GTK passes to
/// `drag-update`/`drag-end` are always relative to the drag's *start* point, not the
/// previous callback), read by `connect_drag_update`/`connect_drag_end`, and cleared
/// when the gesture ends. `Rc<RefCell<Option<_>>>` mirrors `resize_debounce`
/// (`schedule_debounced_resize`) — state that doesn't exist until a gesture starts.
struct DayDragState {
    zone: DayDragZone,
    original_start: DateTime<Local>,
    original_end: DateTime<Local>,
}

/// One drag's proposed new `start`/`end`, plus that day's midnight
/// (`apply_day_drag_geometry` needs it to turn `start` back into a `margin_top` pixel
/// offset, so it's computed once here rather than a second time by every caller).
struct DayDragResult {
    day_start: DateTime<Local>,
    start: DateTime<Local>,
    end: DateTime<Local>,
    /// The drag's proposed day column index into the `dates` slice it was computed
    /// against — unchanged from the card's original column for `ResizeTop`/
    /// `ResizeBottom` (a resize never changes the day) and for the single-column Day
    /// view, and used by `apply_day_drag_geometry` to reposition the card
    /// horizontally when a `Move` drag crosses into a different day column.
    column_index: usize,
}

/// Max pixel thickness of the top/bottom edge-grab zones `classify_day_drag_zone`
/// resolves into `ResizeTop`/`ResizeBottom` — capped at a third of the card's height
/// (see `day_drag_edge_zone_px`) so the two edge zones can never overlap, even at
/// `day_event_block`'s shortest-card floor (`height.max(18)`: 18 / 3 = 6px).
const DAY_EVENT_EDGE_GRAB_PX: i32 = 10;

/// Vertical gap `install_day_event_drag` keeps the drag-modifier hint (`.day-drag-hint`)
/// above the card's current top edge while dragging — a fixed pixel offset rather than a
/// measured one, since the hint's exact height isn't worth measuring for a transient,
/// cosmetic label (contrast `subject_has_room`, which measures `time_bubble` because its
/// width feeds a real layout decision).
const DAY_DRAG_HINT_OFFSET_PX: i32 = 22;

/// The 15-minute floor a drag (move or either resize) can never compress an event
/// past — the same floor `layout_day_events`/`day_event_block` already clamp
/// degenerate-duration events to (`.max(15)`), so a dragged card can't end up shorter
/// than a plain zero-duration DB row would already render at.
const DAY_EVENT_MIN_DURATION_MINUTES: i64 = 15;

/// Total pointer movement (Euclidean, in px) below which `connect_drag_end` treats the
/// gesture as a plain click (open `show_event_popover`) rather than a committed
/// move/resize — distinct from, and much smaller than, `DAY_EVENT_EDGE_GRAB_PX`, which
/// decides *where* on the card a real drag grabbed, not whether one happened at all.
const DAY_EVENT_CLICK_MOVE_THRESHOLD_PX: f64 = 4.0;

/// The top/bottom edge-grab band for a card of `card_height` px, capped so the two
/// zones can never meet in the middle and swallow the `Move` zone entirely.
fn day_drag_edge_zone_px(card_height: i32) -> i32 {
    (card_height / 3).min(DAY_EVENT_EDGE_GRAB_PX).max(1)
}

/// Resolves a press at local `y` (within a card of allocated `card_height`) into which
/// part of the event it grabbed.
fn classify_day_drag_zone(y: f64, card_height: i32) -> DayDragZone {
    let edge = day_drag_edge_zone_px(card_height) as f64;
    if y <= edge {
        DayDragZone::ResizeTop
    } else if y >= card_height as f64 - edge {
        DayDragZone::ResizeBottom
    } else {
        DayDragZone::Move
    }
}

/// Resolves which snap granularity a Day view drag should use *right now*, from the
/// gesture's current modifier state: Ctrl+Shift held snaps to `ctrl_shift_minutes`
/// (`AppSettings::day_drag_snap_ctrl_shift_minutes`, 1 minute by default), plain Ctrl to
/// `ctrl_minutes` (`day_drag_snap_ctrl_minutes`, 5 by default), and neither to
/// `scale_minutes` (the Day view's own time-scale gridlines). Checking the Ctrl+Shift
/// combo before plain Ctrl matters — `state.contains(CONTROL_MASK)` alone is also `true`
/// when Shift is additionally held, so the more specific combo has to be tested first to
/// resolve to the finer tier. Shared by `install_day_event_drag`'s `connect_drag_update`
/// (live feedback) and `connect_drag_end` (commit) so a drag's on-screen granularity and
/// its persisted result always agree.
fn day_drag_snap_scale(
    state: gtk4::gdk::ModifierType,
    scale_minutes: i64,
    ctrl_minutes: i64,
    ctrl_shift_minutes: i64,
) -> Option<i64> {
    if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::SHIFT_MASK) {
        Some(ctrl_shift_minutes)
    } else if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        Some(ctrl_minutes)
    } else {
        Some(scale_minutes)
    }
}

/// Rounds `minutes` (since midnight, possibly fractional/negative) to the nearest
/// multiple of `scale_minutes` — the same increment `populate_day_hour_grid` draws
/// gridlines at, so a snapped drag always lands exactly on a visible line regardless of
/// whether the event's own original time happened to be on-grid already.
fn snap_minutes_since_midnight(minutes: f64, scale_minutes: i64) -> i64 {
    (minutes / scale_minutes as f64).round() as i64 * scale_minutes
}

/// Resolves a raw (fractional, possibly off-grid) minutes-since-midnight value into a
/// whole-minute one: `Some(scale)` snaps to the nearest multiple of `scale` (a visible
/// gridline), `None` just rounds to the nearest whole minute — smooth, continuous
/// tracking with no visible quantization at any sane `pixels_per_minute`, since
/// sub-minute precision isn't distinguishable on screen. Used to keep
/// `compute_day_drag_times`'s live-feedback and commit paths sharing identical
/// clamping logic while differing only in this rounding granularity.
fn resolve_minutes(minutes: f64, scale_minutes: Option<i64>) -> i64 {
    match scale_minutes {
        Some(scale) => snap_minutes_since_midnight(minutes, scale),
        None => minutes.round() as i64,
    }
}

/// Computes this drag's current proposed `(start, end)` from `state`'s pre-drag values
/// plus the gesture's `offset_x`/`offset_y` (px; `offset_y` positive = downward, same
/// convention `card.set_margin_top` uses), clamped to the event's own calendar day and
/// to `DAY_EVENT_MIN_DURATION_MINUTES`. `scale_minutes` (via `resolve_minutes`) controls
/// whether the resulting *absolute* clock time snaps to the nearest gridline
/// (`Some`, used once on commit) or just the nearest whole minute (`None`, used on
/// every live-feedback frame so the drag tracks the pointer smoothly instead of
/// visibly stepping between gridlines). Shared by both paths so they apply identical
/// math to identical inputs, differing only in that one parameter.
///
/// `offset_x` only moves the event to a different day for `DayDragZone::Move` — resize
/// drags always keep the event's original day, matching Google Calendar's own day-view
/// affordances (an edge handle only ever changes duration). The target day is resolved
/// as a *column index* into `dates` (`day_index + round(offset_x / day_column_width)`,
/// clamped to the visible range), not raw calendar-day arithmetic, since `dates` can
/// skip weekends (`five_day_window`) — crossing from Friday's column into the next one
/// should land on Monday, not Saturday.
fn compute_day_drag_times(
    state: &DayDragState,
    offset_x: f64,
    offset_y: f64,
    pixels_per_minute: f64,
    day_column_width: f64,
    dates: &[NaiveDate],
    day_index: usize,
    scale_minutes: Option<i64>,
) -> DayDragResult {
    let raw_delta_minutes = offset_y / pixels_per_minute;

    let target_column = match state.zone {
        DayDragZone::Move if dates.len() > 1 => {
            let column_delta = (offset_x / day_column_width).round() as i64;
            (day_index as i64 + column_delta).clamp(0, dates.len() as i64 - 1) as usize
        }
        _ => day_index,
    };
    let target_date = dates.get(target_column).copied().unwrap_or_else(|| state.original_start.date_naive());

    let midnight = NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is always valid");
    // The pre-drag start/end's time-of-day is always read against *their own* day's
    // midnight, even when the drag has moved the event to a different day's column —
    // `raw_delta_minutes` is a pure time-of-day adjustment, independent of which day
    // it ends up applied to.
    let original_day_start = local_datetime(state.original_start.date_naive(), midnight);
    let day_start = local_datetime(target_date, midnight);
    let day_end = day_start + Duration::days(1);
    let min_duration = Duration::minutes(DAY_EVENT_MIN_DURATION_MINUTES);
    let duration = state.original_end - state.original_start;

    let (start, end) = match state.zone {
        DayDragZone::Move => {
            let original_start_min = (state.original_start - original_day_start).num_minutes() as f64;
            let resolved_min = resolve_minutes(original_start_min + raw_delta_minutes, scale_minutes);
            let start = (day_start + Duration::minutes(resolved_min)).max(day_start).min(day_end - duration);
            (start, start + duration)
        }
        DayDragZone::ResizeTop => {
            let end = state.original_end;
            let original_start_min = (state.original_start - original_day_start).num_minutes() as f64;
            let resolved_min = resolve_minutes(original_start_min + raw_delta_minutes, scale_minutes);
            let start = (day_start + Duration::minutes(resolved_min)).max(day_start).min(end - min_duration);
            (start, end)
        }
        DayDragZone::ResizeBottom => {
            let start = state.original_start;
            let original_end_min = (state.original_end - original_day_start).num_minutes() as f64;
            let resolved_min = resolve_minutes(original_end_min + raw_delta_minutes, scale_minutes);
            let end = (day_start + Duration::minutes(resolved_min)).max(start + min_duration).min(day_end);
            (start, end)
        }
    };

    DayDragResult { day_start, start, end, column_index: target_column }
}

/// Applies `result` to `card`'s geometry and `time_bubble`'s text — exactly the two
/// properties `day_event_block` derives from an event's start/end at build time
/// (`set_margin_top` / `set_size_request`'s height, and the time-bubble label), mutated
/// in place so `connect_drag_update` never sends an `AppMsg` per pointer-move
/// (`install_day_zoom_controller`'s "one message per meaningful action" precedent).
/// Also keeps `drag_hint` (the Ctrl/Ctrl+Shift snap-interval reminder,
/// `install_day_event_drag`'s doc comment) tracking just above `card`'s new top edge —
/// only its position changes here, since its text is fixed at build time.
/// `card_width` is fixed for the whole drag (a vertical drag never changes a card's
/// per-lane column width), so only height/top change — and so is `start_only`
/// (`day_event_block`'s own `subject_has_room` verdict for this card): the label is
/// kept to whichever form the card started the drag in rather than re-measured on every
/// pointer-move, so it doesn't flip formats mid-drag.
///
/// `card`'s horizontal position shifts too, by a plain delta —
/// `card_margin_start + (result.column_index - day_index) * day_column_width` — rather
/// than being recomputed from scratch, so it's correct regardless of how many other
/// events share the target day's lanes (that real layout only gets resolved once the
/// drag commits and `AppMsg::EventUpdated` triggers a full repopulate; live-dragging
/// never reshuffles other cards out of the way, same as it already doesn't vertically).
fn apply_day_drag_geometry(
    card: &gtk4::Box,
    time_bubble: &gtk4::Label,
    drag_hint: &gtk4::Label,
    card_width: i32,
    card_margin_start: i32,
    day_index: usize,
    day_column_width: i32,
    result: &DayDragResult,
    pixels_per_minute: f64,
    time_format: TimeFormat,
    start_only: bool,
) {
    let top = ((result.start - result.day_start).num_minutes() as f64 * pixels_per_minute).round() as i32;
    let height = ((result.end - result.start).num_minutes() as f64 * pixels_per_minute).round() as i32;
    let left = card_margin_start + (result.column_index as i32 - day_index as i32) * day_column_width;
    card.set_margin_top(top);
    card.set_margin_start(left);
    card.set_size_request(card_width, height.max(18));
    drag_hint.set_margin_top((top - DAY_DRAG_HINT_OFFSET_PX).max(0));
    drag_hint.set_margin_start(left);
    time_bubble.set_label(&if start_only {
        format_clock(result.start.time(), time_format)
    } else {
        format!(
            "{}–{}",
            format_clock(result.start.time(), time_format),
            format_clock(result.end.time(), time_format)
        )
    });
}

/// Persists a completed move/resize drag's final start/end for event `event_id`,
/// following the same look-up-detail → resolve-calendar/account → build-`EventEdits`
/// → `update_event` → `AppMsg::EventUpdated` sequence `confirm_delete_event` and
/// `show_edit_event_dialog`'s Save handler already use — every field but `start`/`end`
/// is carried over unchanged from the freshly-loaded `EventDetail`, since `update_event`
/// does a full `UPDATE events SET ...` covering every column, not a partial patch.
/// Always ends by requesting a refresh (`AppMsg::EventUpdated`), success or failure —
/// the card's geometry was already mutated live during the drag independent of
/// storage, so even a failed commit needs `App::refresh` to rebuild the day view from
/// the last-good DB row and snap the card back to reality.
fn commit_day_drag(event_id: i64, result: &DayDragResult, ctx: &EventCtx) {
    let detail = match event_detail(&ctx.storage, event_id) {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            tracing::warn!(event_id, "event no longer exists; discarding drag");
            ctx.sender.input(AppMsg::EventUpdated);
            return;
        }
        Err(err) => {
            tracing::warn!(%err, event_id, "failed to load event after drag");
            ctx.sender.input(AppMsg::EventUpdated);
            return;
        }
    };
    let calendars = calendars_by_account(&ctx.storage).unwrap_or_default();
    let Some(calendar) = calendars.iter().find(|c| c.id == detail.calendar_id) else {
        tracing::warn!(event_id, "no calendar found for event; discarding drag");
        ctx.sender.input(AppMsg::EventUpdated);
        return;
    };

    let edits = EventEdits {
        title: detail.title,
        description: detail.description,
        location: detail.location,
        start: result.start.to_rfc3339(),
        end: result.end.to_rfc3339(),
        all_day: detail.all_day,
        color: detail.color,
        reminders: detail.reminders,
        busy: detail.busy,
        visibility: detail.visibility,
        recurrence: detail.recurrence,
    };
    if let Err(err) = update_event(&ctx.storage, event_id, detail.calendar_id, calendar.account_id, &edits) {
        tracing::warn!(%err, event_id, "failed to save dragged event");
    }
    ctx.sender.input(AppMsg::EventUpdated);
}

/// Replaces `wire_event_click` for Day view's timed event cards (`day_event_block`
/// only — the all-day strip and Month view keep plain `wire_event_click`, out of this
/// feature's scope) with a single drag-aware `GestureDrag`: `connect_drag_begin`
/// classifies the press into a `DayDragZone` (`classify_day_drag_zone`) and snapshots
/// the event's pre-drag start/end; `connect_drag_update` live-mutates `card`'s
/// geometry/time-bubble label on every pointer move (`apply_day_drag_geometry`, snapped
/// per `day_drag_snap_scale` — the current time-scale gridline by default, or one of the
/// two configurable overrides while Ctrl/Ctrl+Shift is held, checked live via
/// `current_event_state()` — same convention `install_day_zoom_controller` uses for its
/// own Ctrl+scroll check) without ever touching `AppMsg`; `connect_drag_end`, if total
/// movement stayed under
/// `DAY_EVENT_CLICK_MOVE_THRESHOLD_PX`, treats it as a plain click — deferring
/// `show_event_popover` by `EVENT_DOUBLE_CLICK_WINDOW` exactly like `wire_event_click`
/// does, so a second such click arriving before that timer fires cancels the pending
/// popover and calls `open_event_editor` instead, rather than committing a move/resize
/// (`commit_day_drag`, snapped the same way — matching whichever granularity was in
/// effect at release).
///
/// The `GestureDrag`/hover `EventControllerMotion` attach to `hit_box`, *not* `card` —
/// deliberately two different widgets. GTK computes a gesture's `offset_x`/`offset_y`
/// by translating each new pointer event into its target widget's *local* coordinate
/// space using that widget's *current* transform, while the drag's start reference was
/// captured once using the *original* transform. If the gesture's target widget were
/// the same one `connect_drag_update` repositions (as an earlier version of this
/// function did), that mutation would keep shifting the very frame the next offset is
/// measured in — a closed feedback loop that made the card lag and flicker between
/// positions instead of tracking the pointer. `hit_box` is built once in
/// `day_event_block` with the same geometry `card` starts at and is never itself
/// moved, so its coordinate frame — and thus every offset GTK reports through it —
/// stays exactly correct for the gesture's whole lifetime; `card` (with
/// `set_can_target(false)`) never owns a controller and is purely what gets moved.
///
/// A single `GestureDrag` (rather than layering a second controller alongside
/// `wire_event_click`'s `GestureClick`) avoids a race between the two:
/// `GestureClick::connect_pressed` fires — and would open the popover — before any
/// drag motion could be detected, since GTK doesn't itself arbitrate between
/// independently-added, ungrouped gesture controllers on the same widget.
///
/// `card_width`/`pixels_per_minute`/`scale_minutes` are `day_event_block`'s own values
/// for this card, threaded straight through rather than re-derived, so the drag always
/// uses the exact geometry/scale the card was last drawn at. `start_only` is that same
/// call's `subject_has_room` verdict, threaded through to `apply_day_drag_geometry` so
/// the live drag preview's time-bubble format matches whatever `time_bubble` was already
/// built with instead of reverting to the full range on the first pointer-move.
fn install_day_event_drag(
    hit_box: &gtk4::Box,
    card: &gtk4::Box,
    time_bubble: &gtk4::Label,
    drag_hint: &gtk4::Label,
    event: &DisplayEvent,
    card_width: i32,
    card_margin_start: i32,
    pixels_per_minute: f64,
    scale_minutes: i64,
    ctrl_snap_minutes: i64,
    ctrl_shift_snap_minutes: i64,
    hold_ms: i64,
    start_only: bool,
    dates: Vec<NaiveDate>,
    day_index: usize,
    day_column_width: i32,
    ctx: &EventCtx,
) {
    let gesture = gtk4::GestureDrag::new();
    let drag_state: Rc<RefCell<Option<DayDragState>>> = Rc::new(RefCell::new(None));
    // The popover for a plain-click release is deferred by `EVENT_DOUBLE_CLICK_WINDOW`
    // rather than shown immediately — see `wire_event_click`'s doc comment for why —
    // so a following double-click can cancel it via this handle before it ever shows.
    let pending_popover: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));
    // `AppSettings::day_drag_hold_ms`'s hold gate: move/resize only actually engages
    // (cursor change, `drag_hint`, and `connect_drag_update` applying geometry) once
    // a press has been held this long — `held_engaged` tracks whether that's
    // happened yet for the gesture currently in progress, and `hold_timer` is the
    // pending timeout that flips it, cancelled on an early release so it can't fire
    // after the gesture it belongs to has already ended.
    let held_engaged: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let hold_timer: Rc<Cell<Option<gtk4::glib::SourceId>>> = Rc::new(Cell::new(None));

    {
        let hit_box = hit_box.clone();
        let drag_hint = drag_hint.clone();
        let event = event.clone();
        let drag_state = drag_state.clone();
        let held_engaged = held_engaged.clone();
        let hold_timer = hold_timer.clone();
        gesture.connect_drag_begin(move |gesture, _start_x, start_y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let start = DateTime::parse_from_rfc3339(&event.start).ok().map(|d| d.with_timezone(&Local));
            let end = DateTime::parse_from_rfc3339(&event.end).ok().map(|d| d.with_timezone(&Local));
            let (Some(original_start), Some(original_end)) = (start, end) else { return };
            let zone = classify_day_drag_zone(start_y, hit_box.height());
            *drag_state.borrow_mut() = Some(DayDragState { zone, original_start, original_end });

            if hold_ms <= 0 {
                held_engaged.set(true);
                hit_box.set_cursor_from_name(Some(match zone {
                    DayDragZone::Move => "grabbing",
                    DayDragZone::ResizeTop | DayDragZone::ResizeBottom => "ns-resize",
                }));
                drag_hint.set_visible(true);
            } else {
                held_engaged.set(false);
                let hit_box = hit_box.clone();
                let drag_hint = drag_hint.clone();
                let held_engaged = held_engaged.clone();
                let drag_state = drag_state.clone();
                let hold_timer_in_timer = hold_timer.clone();
                let id = gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(hold_ms as u64), move || {
                    // The gesture may have already ended (released early) by the time
                    // this fires — only engage if it's still in progress.
                    if drag_state.borrow().is_some() {
                        held_engaged.set(true);
                        // GLib already auto-removed this one-shot source; clear it so
                        // `connect_drag_end` doesn't try to remove it again and panic.
                        hold_timer_in_timer.set(None);
                        hit_box.set_cursor_from_name(Some(match zone {
                            DayDragZone::Move => "grabbing",
                            DayDragZone::ResizeTop | DayDragZone::ResizeBottom => "ns-resize",
                        }));
                        drag_hint.set_visible(true);
                    }
                });
                hold_timer.set(Some(id));
            }
        });
    }

    {
        let card = card.clone();
        let time_bubble = time_bubble.clone();
        let drag_hint = drag_hint.clone();
        let drag_state = drag_state.clone();
        let held_engaged = held_engaged.clone();
        let time_format = ctx.time_format;
        let dates = dates.clone();
        gesture.connect_drag_update(move |gesture, offset_x, offset_y| {
            if !held_engaged.get() {
                return;
            }
            let state_guard = drag_state.borrow();
            let Some(state) = state_guard.as_ref() else { return };
            // Checked live (not latched at drag-begin), so toggling Ctrl/Shift mid-drag
            // flips the snap granularity immediately — same convention
            // `install_day_zoom_controller` already uses for its own Ctrl+scroll check.
            let scale =
                day_drag_snap_scale(gesture.current_event_state(), scale_minutes, ctrl_snap_minutes, ctrl_shift_snap_minutes);
            let result = compute_day_drag_times(
                state,
                offset_x,
                offset_y,
                pixels_per_minute,
                day_column_width as f64,
                &dates,
                day_index,
                scale,
            );
            apply_day_drag_geometry(
                &card,
                &time_bubble,
                &drag_hint,
                card_width,
                card_margin_start,
                day_index,
                day_column_width,
                &result,
                pixels_per_minute,
                time_format,
                start_only,
            );
        });
    }

    {
        let hit_box = hit_box.clone();
        let card = card.clone();
        let drag_hint = drag_hint.clone();
        let event = event.clone();
        let ctx = ctx.clone();
        let drag_state = drag_state.clone();
        let held_engaged = held_engaged.clone();
        let hold_timer = hold_timer.clone();
        let pending_popover = pending_popover.clone();
        let dates = dates.clone();
        gesture.connect_drag_end(move |gesture, offset_x, offset_y| {
            if let Some(id) = hold_timer.take() {
                id.remove();
            }
            hit_box.set_cursor_from_name(Some("pointer"));
            drag_hint.set_visible(false);
            let Some(state) = drag_state.borrow_mut().take() else { return };
            let moved = (offset_x * offset_x + offset_y * offset_y).sqrt();
            if moved < DAY_EVENT_CLICK_MOVE_THRESHOLD_PX {
                if let Some(id) = pending_popover.take() {
                    // A popover from the previous click is still pending: this is its
                    // double-click, so cancel that popover and edit instead.
                    id.remove();
                    open_event_editor(event.id, &ctx);
                    return;
                }
                let card = card.clone();
                let event = event.clone();
                let ctx = ctx.clone();
                let pending_popover_for_timer = pending_popover.clone();
                let id = gtk4::glib::timeout_add_local_once(EVENT_DOUBLE_CLICK_WINDOW, move || {
                    pending_popover_for_timer.set(None);
                    show_event_popover(&card, &event, &ctx);
                });
                pending_popover.set(Some(id));
                return;
            }
            if !held_engaged.get() {
                // Moved far enough not to count as a plain click, but released
                // before `day_drag_hold_ms`'s hold gate engaged — per that setting's
                // contract this does nothing: `connect_drag_update` never applied any
                // geometry while un-engaged, so there's nothing to commit or roll
                // back, the card just never moved.
                return;
            }
            // Commit whichever granularity was in effect at release (mirrors
            // `connect_drag_update`'s own live check, so the persisted value always
            // matches what was last shown on screen).
            let scale =
                day_drag_snap_scale(gesture.current_event_state(), scale_minutes, ctrl_snap_minutes, ctrl_shift_snap_minutes);
            let result = compute_day_drag_times(
                &state,
                offset_x,
                offset_y,
                pixels_per_minute,
                day_column_width as f64,
                &dates,
                day_index,
                scale,
            );
            commit_day_drag(event.id, &result, &ctx);
        });
    }

    hit_box.add_controller(gesture);

    // Hover-only cursor affordance (no drag in progress): "grab" over the body,
    // "ns-resize" over either edge zone, falling back to "pointer" off the card —
    // purely cosmetic, mirrors `hit_box.set_cursor_from_name(Some("pointer"))`'s
    // existing role at `hit_box`'s build time. Skipped while `drag_state` is `Some` so
    // it doesn't fight the "grabbing"/"ns-resize" cursor `connect_drag_begin` already
    // set for the active gesture.
    let motion = gtk4::EventControllerMotion::new();
    {
        let hit_box = hit_box.clone();
        let drag_state = drag_state.clone();
        motion.connect_motion(move |_, _x, y| {
            if drag_state.borrow().is_some() {
                return;
            }
            let zone = classify_day_drag_zone(y, hit_box.height());
            hit_box.set_cursor_from_name(Some(match zone {
                DayDragZone::Move => "grab",
                DayDragZone::ResizeTop | DayDragZone::ResizeBottom => "ns-resize",
            }));
        });
    }
    {
        let hit_box = hit_box.clone();
        motion.connect_leave(move |_| hit_box.set_cursor_from_name(Some("pointer")));
    }
    hit_box.add_controller(motion);
}

/// Scrolls the Day view so "now" isn't right at the very top edge — a few hours of
/// context above the current-time line, mirroring how Google Calendar's day view
/// opens scrolled to roughly this position rather than to midnight. Called only when
/// switching *into* Day view on today's date (`AppMsg::SetView`), not on every
/// `refresh`, so a manual scroll (e.g. after toggling a calendar's visibility) isn't
/// fought on every unrelated message.
fn scroll_day_view_to_now(scroller: &gtk4::ScrolledWindow, scale_minutes: i64) {
    let pixels_per_minute = DAY_ROW_HEIGHT_PX as f64 / scale_minutes as f64;
    let now = Local::now();
    let now_px = (now.hour() as f64 * 60.0 + now.minute() as f64) * pixels_per_minute;
    let target = (now_px - 3.0 * 60.0 * pixels_per_minute).max(0.0);
    scroller.vadjustment().set_value(target);
}

/// Looks up `event_id`'s full `EventDetail` and the current calendar list, then opens
/// `show_edit_event_dialog` pre-filled with it — the "jump straight to editing" action
/// shared by the event popover's Edit button and a double-click on an event
/// (`wire_event_click`/`install_day_event_drag`), so both paths resolve the event the
/// same way instead of duplicating the lookup.
fn open_event_editor(event_id: i64, ctx: &EventCtx) {
    match event_detail(&ctx.storage, event_id) {
        Ok(Some(detail)) => {
            let calendars = calendars_by_account(&ctx.storage).unwrap_or_default();
            show_edit_event_dialog(detail, calendars, ctx.clone());
        }
        Ok(None) => tracing::warn!(event_id, "event no longer exists; cannot edit"),
        Err(err) => tracing::warn!(%err, event_id, "failed to load event for editing"),
    }
}

/// Builds and pops up the event-detail card: a colored dot + title, the formatted
/// date/time, then location/description/calendar rows for whichever of those the
/// event actually has, topped by a header strip of Edit/Delete/More/Close controls.
/// Edit opens `show_edit_event_dialog` pre-filled with this event; Delete asks for
/// confirmation (`confirm_delete_event`) before removing it. More stays
/// visible-but-disabled ("coming soon", matching the header bar's Preferences button
/// and view-switcher menu) since that menu isn't built yet. Parented to
/// `anchor` (the clicked event row) so it floats next to whatever was clicked, and
/// unparents itself on close so repeated clicks on the same row don't accumulate
/// hidden popovers in the widget tree.
fn show_event_popover(anchor: &gtk4::Box, event: &DisplayEvent, ctx: &EventCtx) {
    let popover = gtk4::Popover::new();
    popover.add_css_class("event-popover");
    popover.set_parent(anchor);
    popover.connect_closed(|popover| popover.unparent());

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.set_width_request(320);

    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    controls.set_halign(gtk4::Align::End);
    controls.set_margin_top(6);
    controls.set_margin_end(6);

    let edit_btn = gtk4::Button::from_icon_name("document-edit-symbolic");
    edit_btn.add_css_class("flat");
    edit_btn.set_tooltip_text(Some("Edit event"));
    {
        let ctx = ctx.clone();
        let event_id = event.id;
        let popover = popover.clone();
        edit_btn.connect_clicked(move |_| {
            popover.popdown();
            open_event_editor(event_id, &ctx);
        });
    }
    controls.append(&edit_btn);

    let delete_btn = gtk4::Button::from_icon_name("user-trash-symbolic");
    delete_btn.add_css_class("flat");
    delete_btn.set_tooltip_text(Some("Delete event"));
    {
        let ctx = ctx.clone();
        let event_id = event.id;
        let event_title = event.title.clone();
        let popover = popover.clone();
        delete_btn.connect_clicked(move |_| {
            popover.popdown();
            confirm_delete_event(&event_title, event_id, ctx.clone());
        });
    }
    controls.append(&delete_btn);

    let more_btn = gtk4::Button::from_icon_name("view-more-symbolic");
    more_btn.add_css_class("flat");
    more_btn.set_sensitive(false);
    more_btn.set_tooltip_text(Some("More options (coming soon)"));
    controls.append(&more_btn);

    let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.set_tooltip_text(Some("Close"));
    {
        let popover = popover.clone();
        close_btn.connect_clicked(move |_| popover.popdown());
    }
    controls.append(&close_btn);

    root.append(&controls);

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.set_margin_bottom(18);
    body.set_margin_top(2);

    let title_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("event-popover-dot");
    dot.set_tooltip_text(Some(&event.calendar_name));
    if let Some(color) = &event.color {
        dot.add_css_class(&css_class_for_color(color));
    }
    title_row.append(&dot);
    let title_label = gtk4::Label::new(Some(&event.title));
    title_label.add_css_class("event-popover-title");
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_xalign(0.0);
    title_label.set_wrap(true);
    title_label.set_hexpand(true);
    title_row.append(&title_label);
    body.append(&title_row);

    let when_label = gtk4::Label::new(Some(&format_event_when(event, ctx.date_format, ctx.time_format)));
    when_label.add_css_class("dim-label");
    when_label.add_css_class("event-popover-when");
    when_label.set_halign(gtk4::Align::Start);
    when_label.set_xalign(0.0);
    body.append(&when_label);

    if let Some(location) = event.location.as_deref().filter(|s| !s.is_empty()) {
        body.append(&detail_row("mark-location-symbolic", location));
    }
    if let Some(description) = event.description.as_deref().filter(|s| !s.is_empty()) {
        body.append(&detail_row("view-list-symbolic", description));
    }

    // `DisplayEvent` (what this whole function is otherwise built from) has no
    // busy/attendee data — its backing query never selects `transparency`/`visibility`/
    // `event_attendees` — so getting either means a second lookup here, the same one
    // the Edit button above already does. `Ok(None)`/`Err` just mean this section is
    // skipped (event deleted between click and render, or a storage hiccup); the rest
    // of the popover still renders fine from `event`/`DisplayEvent` alone.
    match event_detail(&ctx.storage, event.id) {
        Ok(Some(detail)) => {
            let busy_text = match detail.busy {
                EventBusyStatus::Busy => "Busy",
                EventBusyStatus::Free => "Free",
            };
            body.append(&detail_row("view-reveal-symbolic", busy_text));

            if !detail.attendees.is_empty() {
                body.append(&guest_list_section(&detail.attendees, detail.organizer_email.as_deref()));
            }

            for reminder in &detail.reminders {
                body.append(&detail_row("alarm-symbolic", &format_reminder_lead_time(reminder)));
            }

            if let Some(link) = detail.hangout_link.as_deref().filter(|s| !s.is_empty()) {
                // Not `detail_row` — that helper renders plain text, and this needs a
                // real clickable action, so it's built directly with the same
                // `.event-popover-detail-row` icon+content layout for visual
                // consistency with the rows around it.
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                row.add_css_class("event-popover-detail-row");

                let icon = gtk4::Image::from_icon_name("camera-web-symbolic");
                icon.add_css_class("dim-label");
                icon.set_valign(gtk4::Align::Start);
                row.append(&icon);

                let join_btn = gtk4::Button::with_label("Join video call");
                join_btn.add_css_class("flat");
                join_btn.set_halign(gtk4::Align::Start);
                let link = link.to_string();
                join_btn.connect_clicked(move |_| {
                    if let Err(err) = gtk4::gio::AppInfo::launch_default_for_uri(&link, None::<&gtk4::gio::AppLaunchContext>)
                    {
                        tracing::warn!(%err, "failed to open video call link");
                    }
                });
                row.append(&join_btn);

                body.append(&row);
            }

            for attachment in &detail.attachments {
                let label = attachment.title.as_deref().filter(|s| !s.is_empty()).unwrap_or(&attachment.file_url);
                body.append(&detail_row("mail-attachment-symbolic", label));
            }
        }
        Ok(None) => {
            tracing::warn!(event_id = event.id, "event no longer exists; skipping busy/guest details in popover")
        }
        Err(err) => tracing::warn!(%err, event_id = event.id, "failed to load event detail for popover"),
    }

    body.append(&detail_row("x-office-calendar-symbolic", &event.calendar_name));

    root.append(&body);
    popover.set_child(Some(&root));
    popover.popup();
}

/// Builds and pops up a month cell's "N more" hover card: `date`'s full event list (not
/// just the hidden tail past `compute_max_visible_events`), each rendered as the same
/// `event_row` chip the month grid itself uses and wired the same way
/// (`wire_event_click`), so clicking one opens the normal `show_event_popover`/editor —
/// this popover only adds a day-scoped preview, not a second way to view event details.
/// Parented to `anchor` (the "N more" label) like `show_event_popover` is to its event
/// row, and left un-autohidden by hover/leave logic entirely: the caller
/// (`wire_day_summary_hover`) owns when this closes, via the returned `Popover`'s
/// `.popdown()`. A day with many events scrolls (`max_content_height`) rather than
/// growing the popover unboundedly.
fn show_day_summary_popover(anchor: &gtk4::Label, date: NaiveDate, day_events: &[DisplayEvent], ctx: &EventCtx) -> gtk4::Popover {
    let popover = gtk4::Popover::new();
    popover.add_css_class("day-summary-popover");
    popover.set_parent(anchor);
    popover.connect_closed(|popover| popover.unparent());

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    root.set_width_request(280);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(14);
    root.set_margin_end(14);

    let header = gtk4::Label::new(Some(&format_popover_date(date, ctx.date_format)));
    header.add_css_class("day-summary-header");
    header.set_halign(gtk4::Align::Start);
    header.set_xalign(0.0);
    root.append(&header);

    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    for event in day_events {
        let row = event_row(event, ctx.time_format);
        wire_event_click(&row, event, ctx);
        list.append(&row);
    }

    let scroller = gtk4::ScrolledWindow::builder()
        .max_content_height(320)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list)
        .build();
    root.append(&scroller);

    popover.set_child(Some(&root));
    popover.popup();
    popover
}

/// Pops up a "Delete event?" confirmation (GNOME HIG: a destructive action with no
/// undo path needs one) before actually removing it. Only on the "delete" response
/// does this look the event back up (`event_detail`, for its `calendar_id`) and its
/// calendar (for the account whose `pending_edits` queue owns the deletion) and call
/// `delete_event` — mirroring how `show_edit_event_dialog`'s Save button resolves
/// those same two pieces of context.
fn confirm_delete_event(title: &str, event_id: i64, ctx: EventCtx) {
    let dialog = adw::AlertDialog::new(Some("Delete event?"), Some(&format!("\u{201c}{title}\u{201d} will be permanently deleted.")));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose(Some(&ctx.window), gtk4::gio::Cancellable::NONE, move |response| {
        if response != "delete" {
            return;
        }
        match event_detail(&ctx.storage, event_id) {
            Ok(Some(detail)) => {
                let calendars = calendars_by_account(&ctx.storage).unwrap_or_default();
                let Some(calendar) = calendars.iter().find(|c| c.id == detail.calendar_id) else {
                    tracing::warn!(event_id, "no calendar found for event; cannot delete");
                    return;
                };
                if let Err(err) = delete_event(&ctx.storage, event_id, detail.calendar_id, calendar.account_id) {
                    tracing::warn!(%err, event_id, "failed to delete event");
                } else {
                    ctx.sender.input(AppMsg::EventUpdated);
                }
            }
            Ok(None) => tracing::warn!(event_id, "event no longer exists; nothing to delete"),
            Err(err) => tracing::warn!(%err, event_id, "failed to load event for deletion"),
        }
    });
}

/// Opens the edit-event dialog (DESIGN_SPEC.md §10's event editor: title, time with an
/// all-day toggle, location, description, and a calendar picker) pre-filled with
/// `event`'s current values. "Does not repeat" and "Add notification" are shown but
/// disabled, matching the app's existing "coming soon" convention (recurring-event
/// editing and the Notification Scheduler are both later roadmap items, §12/§20) — the
/// affordance is visible before the feature behind it exists.
///
/// Saving writes straight to the local `events` table and queues a `pending_edits` row
/// (§9) — there's no live push to Google yet (that's roadmap phase 3); the local store
/// stays the UI's only source of truth (§5) either way, so the change shows up
/// immediately regardless.
///
/// Doubles as the header bar's Create dialog: `event.id == 0` (never a real row id,
/// since `events.id` is an autoincrementing primary key) marks a not-yet-saved event —
/// see `default_new_event` — and the Save handler below branches on that to insert a
/// new row instead of updating an existing one.
fn show_edit_event_dialog(event: EventDetail, calendars: Vec<CalendarSummary>, ctx: EventCtx) {
    let (start_date, start_time) = split_date_time(&event.start, event.all_day);
    let (mut end_date, end_time) = split_date_time(&event.end, event.all_day);
    if event.all_day {
        // Google's all-day `end` is exclusive (the day *after* the last day of the
        // event) — shown to the user as the inclusive last day instead, and converted
        // back on save.
        end_date = end_date.pred_opt().unwrap_or(end_date).max(start_date);
    }

    // The panel's own root: an embedded widget (never a separate top-level window —
    // see DESIGN_SPEC.md §10's redesigned dockable editor) that gets reparented
    // across floating/bottom/right at runtime instead of being torn down and
    // rebuilt on every mode change. `mode` tracks which of the three is currently
    // active so `install_panel_drag`'s state machine and the initial `dock_panel`
    // call below agree on it.
    let panel_root = adw::ToolbarView::new();
    let mode = Rc::new(Cell::new(EventEditorPanelMode::Floating));

    // Tracks whether anything has actually changed since the dialog opened, so
    // closing it (via Escape or the header's close button) can ask before throwing
    // away an in-progress edit — but close silently when nothing was touched,
    // matching the pre-fill signals below being wired up *after* every field's
    // initial value is set.
    let dirty = Rc::new(Cell::new(false));

    // A slim strip above the controls row — a real window has a titlebar to grab
    // regardless of what's in its toolbar underneath, so this panel gets one too,
    // and it's the one and only widget `install_panel_drag`'s `GestureDrag` attaches
    // to (both floating and docked — undocking needs a drag handle just as much as
    // floating does). Left blank (no title text) — the actual editable title field
    // lives in the row underneath, which already reads as "this is titled X".
    let titlebar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    titlebar.add_css_class("event-editor-titlebar");
    titlebar.set_margin_start(10);
    titlebar.set_margin_end(10);
    titlebar.set_margin_top(4);
    titlebar.set_margin_bottom(4);
    titlebar.set_cursor_from_name(Some("grab"));

    // A plain `Box`, not `adw::HeaderBar` — deliberately, even though it means giving
    // up `HeaderBar`'s automatic centered-title layout. `HeaderBar`/`AdwHeaderBar`
    // wires up "drag empty space to move the window" unconditionally, targeting
    // whatever toplevel surface it happens to live under — harmless when it's a
    // window's own titlebar, but this header lives inside the *main* window's
    // surface, so that built-in drag would move the whole app window instead of
    // just this panel. `notifications.rs`'s floating dialog avoids the same
    // conflict the same way: a plain `Box` has no built-in window-drag behavior of
    // its own to fight.
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    header.add_css_class("event-editor-header");
    header.set_margin_all(6);

    let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.set_tooltip_text(Some("Close"));
    {
        let panel_root = panel_root.clone();
        let docks = ctx.docks.clone();
        let window = ctx.window.clone();
        let dirty = dirty.clone();
        close_btn.connect_clicked(move |_| {
            let panel_root = panel_root.clone();
            let docks = docks.clone();
            confirm_close(&window, &dirty, move || close_panel(&panel_root, &docks));
        });
    }
    header.append(&close_btn);

    let title_entry = gtk4::Entry::new();
    // `.flat` alone still leaves a visible border/focus outline in this theme —
    // `.event-editor-title-entry` (load_static_css) explicitly zeroes out
    // border/background/box-shadow in every state so it truly reads as inline text
    // rather than a boxed form field sitting in the header strip.
    title_entry.add_css_class("flat");
    title_entry.add_css_class("event-editor-title-entry");
    title_entry.set_placeholder_text(Some("Add title"));
    title_entry.set_text(&event.title);
    title_entry.set_hexpand(true);
    {
        let dirty = dirty.clone();
        title_entry.connect_changed(move |_| dirty.set(true));
    }
    // Not centered the way `HeaderBar::set_title_widget` would center it — a plain
    // `Box` just lays children out left-to-right — but it reads fine for an embedded
    // panel that was never trying to look like a titlebar in the first place.
    header.append(&title_entry);

    let save_btn = gtk4::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    header.append(&save_btn);
    header.append(&more_actions_button(&event, &calendars, &ctx, &panel_root));

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    body.set_margin_all(18);

    let (start_button, start_picked) = date_picker(start_date, dirty.clone(), ctx.date_format);
    let (end_button, end_picked) = date_picker(end_date, dirty.clone(), ctx.date_format);

    let (start_time_entry, start_time_picked) = time_picker(start_time, dirty.clone(), ctx.time_format);
    let (end_time_entry, end_time_picked) = time_picker(end_time, dirty.clone(), ctx.time_format);

    let time_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    time_row.append(&start_time_entry);
    time_row.append(&gtk4::Label::new(Some("to")));
    time_row.append(&end_time_entry);
    time_row.set_visible(!event.all_day);

    let date_time_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    date_time_row.append(&start_button);
    date_time_row.append(&time_row);
    date_time_row.append(&end_button);

    let options_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let all_day_check = gtk4::CheckButton::with_label("All day");
    all_day_check.set_active(event.all_day);
    {
        let time_row = time_row.clone();
        let dirty = dirty.clone();
        all_day_check.connect_toggled(move |btn| {
            time_row.set_visible(!btn.is_active());
            dirty.set(true);
        });
    }
    options_row.append(&all_day_check);

    let (repeat_btn, recurrence_state) =
        build_repeat_control(&event, start_picked.clone(), dirty.clone(), ctx.date_format, &ctx.window);
    options_row.append(&repeat_btn);

    let location_entry = gtk4::Entry::new();
    location_entry.set_placeholder_text(Some("Add location"));
    location_entry.set_text(event.location.as_deref().unwrap_or(""));
    {
        let dirty = dirty.clone();
        location_entry.connect_changed(move |_| dirty.set(true));
    }
    let location_row = field_row("mark-location-symbolic", "Location", &location_entry);

    // Built here (right after Location, matching where the field naturally reads in
    // the code) but not appended to `body` until after the calendar/color,
    // notifications, and busy/visibility rows below — DESIGN_SPEC's reference
    // screenshots put the description field last, and only the `body.append` call
    // order (not construction order) affects the dialog's visual layout.
    let description_view = gtk4::TextView::new();
    description_view.set_wrap_mode(gtk4::WrapMode::WordChar);
    let description_tags = install_description_tags(&description_view.buffer());
    load_description_markup(&description_view.buffer(), event.description.as_deref().unwrap_or(""), &description_tags);
    {
        let dirty = dirty.clone();
        description_view.buffer().connect_changed(move |_| dirty.set(true));
    }
    let description_toolbar = build_description_toolbar(&description_view, &description_tags, &dirty);
    let description_frame = gtk4::ScrolledWindow::builder()
        .min_content_height(96)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&description_view)
        .build();
    description_frame.add_css_class("description-frame");
    let description_container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    description_container.append(&description_toolbar);
    description_container.append(&description_frame);
    let description_row = field_row("view-list-symbolic", "Description", &description_container);

    let calendar_names: Vec<&str> = calendars.iter().map(|c| c.display_name.as_str()).collect();
    let calendar_dropdown = gtk4::DropDown::from_strings(&calendar_names);
    calendar_dropdown.set_hexpand(true);
    if let Some(index) = calendars.iter().position(|c| c.id == event.calendar_id) {
        calendar_dropdown.set_selected(index as u32);
    }
    {
        let dirty = dirty.clone();
        calendar_dropdown.connect_selected_notify(move |_| dirty.set(true));
    }
    let (event_color_button, event_color_state) = event_color_picker(event.color.clone(), dirty.clone());
    let calendar_and_color_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    calendar_and_color_row.append(&calendar_dropdown);
    calendar_and_color_row.append(&event_color_button);
    let calendar_row = field_row("x-office-calendar-symbolic", "Calendar", &calendar_and_color_row);

    // Reminders (reference screenshots): a list of "[method ▾] [qty] [unit ▾] [✕]"
    // rows, pre-filled from `event.reminders`, plus an "Add notification" button that
    // appends more (up to `MAX_REMINDERS`, matching Google Calendar's own per-event
    // cap) — see `add_reminder_row`.
    let notif_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let reminder_rows: Rc<RefCell<Vec<ReminderRow>>> = Rc::new(RefCell::new(Vec::new()));
    let reminders_list = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    notif_box.append(&reminders_list);

    let add_notif_btn = gtk4::Button::with_label("Add notification");
    add_notif_btn.add_css_class("flat");
    add_notif_btn.set_halign(gtk4::Align::Start);

    for reminder in &event.reminders {
        add_reminder_row(&reminders_list, &reminder_rows, &add_notif_btn, &dirty, reminder);
    }
    add_notif_btn.set_sensitive(event.reminders.len() < MAX_REMINDERS);

    {
        let reminders_list = reminders_list.clone();
        let reminder_rows = reminder_rows.clone();
        let dirty = dirty.clone();
        add_notif_btn.connect_clicked(move |btn| {
            add_reminder_row(
                &reminders_list,
                &reminder_rows,
                btn,
                &dirty,
                &EventReminder {
                    method: ReminderMethod::Popup,
                    minutes: 10,
                },
            );
            btn.set_sensitive(reminder_rows.borrow().len() < MAX_REMINDERS);
            dirty.set(true);
        });
    }
    notif_box.append(&add_notif_btn);

    let notif_hint = gtk4::Label::new(Some("Notifications only apply to you."));
    notif_hint.add_css_class("dim-label");
    notif_hint.set_halign(gtk4::Align::Start);
    notif_box.append(&notif_hint);
    let notifications_row = field_row("alarm-symbolic", "Notifications", &notif_box);

    // Busy/Free and visibility (reference screenshot): both local-only for now, like
    // the color override above — `events.transparency`/`events.visibility` round-trip
    // through `pending_edits` (§9) the same way, ready for a future sync push.
    let busy_dropdown = gtk4::DropDown::from_strings(&["Busy", "Free"]);
    busy_dropdown.set_selected(match event.busy {
        EventBusyStatus::Busy => 0,
        EventBusyStatus::Free => 1,
    });
    {
        let dirty = dirty.clone();
        busy_dropdown.connect_selected_notify(move |_| dirty.set(true));
    }

    let visibility_dropdown = gtk4::DropDown::from_strings(&["Default visibility", "Public", "Private"]);
    visibility_dropdown.set_hexpand(true);
    visibility_dropdown.set_selected(match event.visibility {
        EventVisibility::Default => 0,
        EventVisibility::Public => 1,
        // `Confidential` has no dropdown entry of its own (DESIGN_SPEC.md §10's editor
        // only offers Google Calendar's own three visibility choices) — it lands on
        // the closest and, in the safe direction, "Private" slot rather than widening
        // to "Public" or losing the restriction entirely by falling back to "Default".
        EventVisibility::Private | EventVisibility::Confidential => 2,
    });
    {
        let dirty = dirty.clone();
        visibility_dropdown.connect_selected_notify(move |_| dirty.set(true));
    }

    let visibility_help = gtk4::Image::from_icon_name("dialog-question-symbolic");
    visibility_help.add_css_class("dim-label");
    visibility_help.set_tooltip_text(Some(
        "Default visibility follows your calendar's sharing settings. Public events are visible to \
         anyone who can view this calendar; private events show only busy/free time to others.",
    ));

    let busy_visibility_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    busy_visibility_row.append(&busy_dropdown);
    busy_visibility_row.append(&visibility_dropdown);
    busy_visibility_row.append(&visibility_help);
    let busy_visibility_field_row = field_row("view-reveal-symbolic", "Busy/visibility", &busy_visibility_row);

    // Every field row now exists as its own free-standing `Box` — none of them have
    // been appended to `body` yet. `relayout_editor_body` (called below, once the
    // panel's starting mode is known, and again on every live dock-mode change) owns
    // arranging them into `body` from here on.
    let body_rows = EditorBodyRows {
        date_time_row: date_time_row.clone(),
        options_row: options_row.clone(),
        location_row,
        calendar_row,
        notifications_row,
        busy_visibility_row: busy_visibility_field_row,
        description_row,
    };
    let relayout_body: Rc<dyn Fn(EventEditorPanelMode)> = {
        let body = body.clone();
        Rc::new(move |mode| relayout_editor_body(&body, &body_rows, mode))
    };

    let scroller = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&body)
        .build();

    panel_root.add_top_bar(&titlebar);
    panel_root.add_top_bar(&header);
    panel_root.set_content(Some(&scroller));
    install_panel_escape_to_close(&panel_root, {
        let panel_root = panel_root.clone();
        let docks = ctx.docks.clone();
        let window = ctx.window.clone();
        let dirty = dirty.clone();
        move || {
            let panel_root = panel_root.clone();
            let docks = docks.clone();
            confirm_close(&window, &dirty, move || close_panel(&panel_root, &docks));
        }
    });

    let event_id = event.id;
    {
        let panel_root = panel_root.clone();
        let docks = ctx.docks.clone();
        // Cloned rather than capturing `ctx` itself — `ctx.storage`/`ctx.docks` are
        // read again after this closure (the `dock_panel`/`install_panel_drag` calls
        // at the end of this function), and a `move` closure would otherwise move
        // those fields out of `ctx` here, since `Storage`/`ComponentSender` aren't
        // `Copy`.
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        let description_tags = description_tags.clone();
        save_btn.connect_clicked(move |_| {
            let title = title_entry.text().trim().to_string();
            let location = non_empty(location_entry.text().to_string());
            let description = non_empty(serialize_description(&description_view.buffer(), &description_tags));
            let all_day = all_day_check.is_active();

            let busy = match busy_dropdown.selected() {
                1 => EventBusyStatus::Free,
                _ => EventBusyStatus::Busy,
            };
            let visibility = match visibility_dropdown.selected() {
                1 => EventVisibility::Public,
                2 => EventVisibility::Private,
                _ => EventVisibility::Default,
            };

            let picked_start_date = start_picked.get();
            let picked_end_date = end_picked.get().max(picked_start_date);

            let (start, end) = if all_day {
                let end_exclusive = picked_end_date + Duration::days(1);
                (
                    picked_start_date.format("%Y-%m-%d").to_string(),
                    end_exclusive.format("%Y-%m-%d").to_string(),
                )
            } else {
                let start_time = start_time_picked.get();
                let end_time = end_time_picked.get();
                let start_dt = local_datetime(picked_start_date, start_time);
                let end_dt = local_datetime(picked_end_date, end_time);
                let end_dt = if end_dt <= start_dt { start_dt + Duration::minutes(30) } else { end_dt };
                (start_dt.to_rfc3339(), end_dt.to_rfc3339())
            };

            let Some(calendar) = calendars.get(calendar_dropdown.selected() as usize) else {
                tracing::warn!(event_id, "no calendar selected in edit dialog; discarding save");
                // `close_panel`, not `confirm_close` — Save (successful or not) is the
                // user's own resolution of the dialog, so it should never trigger the
                // dirty "discard changes?" guard `confirm_close` would otherwise run into.
                close_panel(&panel_root, &docks);
                return;
            };

            let color = event_color_state.borrow().clone();
            let reminders: Vec<EventReminder> = reminder_rows
                .borrow()
                .iter()
                .map(|row| {
                    let method = match row.method_dropdown.selected() {
                        1 => ReminderMethod::Email,
                        _ => ReminderMethod::Popup,
                    };
                    let quantity = row.quantity_spin.value_as_int() as i64;
                    let multiplier = REMINDER_UNITS[row.unit_dropdown.selected() as usize].1;
                    EventReminder {
                        method,
                        minutes: quantity * multiplier,
                    }
                })
                .collect();

            let recurrence = recurrence_state.borrow().as_ref().map(Recurrence::to_rrule_string);

            let edits = EventEdits {
                title,
                description,
                location,
                start,
                end,
                all_day,
                color,
                reminders,
                busy,
                visibility,
                recurrence,
            };
            let result = if event_id > 0 {
                update_event(&storage, event_id, calendar.id, calendar.account_id, &edits)
            } else {
                create_event(&storage, calendar.id, calendar.account_id, &edits).map(|_| ())
            };
            if let Err(err) = result {
                tracing::warn!(%err, event_id, "failed to save event");
            } else {
                sender.input(AppMsg::EventUpdated);
            }
            close_panel(&panel_root, &docks);
        });
    }

    let settings = load_settings(&ctx.storage).unwrap_or_default();
    mode.set(settings.event_editor_panel_mode);
    dock_panel(&panel_root, mode.get(), &ctx.docks, &ctx.storage);
    relayout_body(mode.get());
    // Makes this panel visible to the one persistent drag gesture installed on
    // `editor_overlay` at `init` time — see `install_panel_drag`'s doc comment for
    // why that gesture lives there instead of on this panel's own titlebar.
    *ctx.docks.current_panel.borrow_mut() = Some(OpenEditorPanel {
        root: panel_root.clone(),
        titlebar: titlebar.clone(),
        mode: mode.clone(),
        relayout: relayout_body,
    });
}

/// Builds the edit dialog's "More actions" header button — a flat, icon-only
/// `MenuButton` next to Save, mirroring Google Calendar's own event-editor overflow
/// menu. Duplicate and Delete are wired to the same plumbing `show_event_popover` and
/// `confirm_delete_event` already use; Copy to another calendar and Print
/// (`copy_to_calendar_button`, `print_event`) are their own real implementations. The
/// remaining Google Calendar actions (Publish event, Change owner) have no equivalent
/// yet, so unlike the others they don't even get a disabled row here.
///
/// Duplicate/Delete/Print act on `event` as last loaded/saved, not on any in-progress
/// edits in the dialog's fields — same as closing the dialog without saving would
/// discard those edits anyway.
fn more_actions_button(event: &EventDetail, calendars: &[CalendarSummary], ctx: &EventCtx, panel_root: &adw::ToolbarView) -> gtk4::MenuButton {
    let button = gtk4::MenuButton::new();
    button.set_label("More actions");
    button.add_css_class("flat");

    let popover = gtk4::Popover::new();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    root.set_margin_all(6);
    root.set_width_request(220);

    // A not-yet-saved event (the "Create event" dialog reusing this same function,
    // see `default_new_event`) has nothing on disk yet to duplicate or delete.
    let has_saved_event = event.id > 0;

    let duplicate_btn = menu_row_button("Duplicate");
    duplicate_btn.set_sensitive(has_saved_event);
    if has_saved_event {
        let ctx = ctx.clone();
        let panel_root = panel_root.clone();
        let popover = popover.clone();
        let calendar_id = event.calendar_id;
        let account_id = calendars.iter().find(|c| c.id == calendar_id).map(|c| c.account_id);
        let edits = EventEdits {
            title: event.title.clone(),
            description: event.description.clone(),
            location: event.location.clone(),
            start: event.start.clone(),
            end: event.end.clone(),
            all_day: event.all_day,
            color: event.color.clone(),
            reminders: event.reminders.clone(),
            busy: event.busy,
            visibility: event.visibility,
            recurrence: event.recurrence.clone(),
        };
        duplicate_btn.connect_clicked(move |_| {
            popover.popdown();
            let Some(account_id) = account_id else {
                tracing::warn!(calendar_id, "no account found for calendar; cannot duplicate event");
                return;
            };
            match create_event(&ctx.storage, calendar_id, account_id, &edits) {
                Ok(_) => {
                    ctx.sender.input(AppMsg::EventUpdated);
                    close_panel(&panel_root, &ctx.docks);
                }
                Err(err) => tracing::warn!(%err, calendar_id, "failed to duplicate event"),
            }
        });
    } else {
        duplicate_btn.set_tooltip_text(Some("Save the event first"));
    }
    root.append(&duplicate_btn);

    root.append(&copy_to_calendar_button(event, calendars, ctx, panel_root));

    let print_btn = menu_row_button("Print");
    print_btn.set_sensitive(has_saved_event);
    if has_saved_event {
        let popover = popover.clone();
        let window = ctx.window.clone();
        let event = event.clone();
        let calendar_name = calendars.iter().find(|c| c.id == event.calendar_id).map(|c| c.display_name.clone());
        let date_format = ctx.date_format;
        let time_format = ctx.time_format;
        print_btn.connect_clicked(move |_| {
            popover.popdown();
            print_event(&event, calendar_name.as_deref(), date_format, time_format, &window);
        });
    } else {
        print_btn.set_tooltip_text(Some("Save the event first"));
    }
    root.append(&print_btn);

    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let delete_btn = menu_row_button("Delete event");
    delete_btn.add_css_class("destructive-action");
    delete_btn.set_sensitive(has_saved_event);
    if has_saved_event {
        let ctx = ctx.clone();
        let panel_root = panel_root.clone();
        let popover = popover.clone();
        let event_id = event.id;
        let event_title = event.title.clone();
        delete_btn.connect_clicked(move |_| {
            popover.popdown();
            // Same reasoning as Save's `close_panel` above: clicking Delete is the
            // user's own resolution of this dialog, so it shouldn't trip the
            // dirty-guard "discard changes?" prompt. `confirm_delete_event` still asks
            // before the deletion itself happens, same as the event popover's delete
            // button.
            close_panel(&panel_root, &ctx.docks);
            confirm_delete_event(&event_title, event_id, ctx.clone());
        });
    } else {
        delete_btn.set_tooltip_text(Some("Save the event first"));
    }
    root.append(&delete_btn);

    popover.set_child(Some(&root));
    button.set_popover(Some(&popover));
    button
}

/// Builds `more_actions_button`'s "Copy to another calendar" row — a nested
/// `MenuButton` whose own popover lists every *other* calendar (the event's current
/// one is left out, since that's just `Duplicate`), each one copying the event there
/// via `create_event` and leaving the original untouched. Past 10 candidates a
/// `SearchEntry` filters the list as you type, since scanning a long list by eye stops
/// being faster than typing a few letters of the target calendar's name right about
/// there.
fn copy_to_calendar_button(event: &EventDetail, calendars: &[CalendarSummary], ctx: &EventCtx, panel_root: &adw::ToolbarView) -> gtk4::MenuButton {
    let label = gtk4::Label::new(Some("Copy to another calendar"));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    let arrow = gtk4::Image::from_icon_name("go-next-symbolic");
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    row.append(&label);
    row.append(&arrow);

    let button = gtk4::MenuButton::new();
    button.set_child(Some(&row));
    button.add_css_class("flat");
    button.add_css_class("customizer-menu-row");

    // Same "nothing saved yet to act on" gating as Duplicate/Delete above.
    if event.id <= 0 {
        button.set_sensitive(false);
        button.set_tooltip_text(Some("Save the event first"));
        return button;
    }

    let targets: Vec<&CalendarSummary> = calendars.iter().filter(|c| c.id != event.calendar_id).collect();
    if targets.is_empty() {
        button.set_sensitive(false);
        button.set_tooltip_text(Some("No other calendars to copy to"));
        return button;
    }

    let popover = gtk4::Popover::new();
    popover.set_position(gtk4::PositionType::Right);
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    root.set_margin_all(6);
    root.set_width_request(240);

    let list_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let mut filter_rows: Vec<(String, gtk4::Button)> = Vec::new();

    for calendar in &targets {
        let calendar_btn = menu_row_button(&calendar.display_name);
        {
            let ctx = ctx.clone();
            let panel_root = panel_root.clone();
            let popover = popover.clone();
            let target_calendar_id = calendar.id;
            let target_account_id = calendar.account_id;
            let edits = EventEdits {
                title: event.title.clone(),
                description: event.description.clone(),
                location: event.location.clone(),
                start: event.start.clone(),
                end: event.end.clone(),
                all_day: event.all_day,
                color: event.color.clone(),
                reminders: event.reminders.clone(),
                busy: event.busy,
                visibility: event.visibility,
                recurrence: event.recurrence.clone(),
            };
            calendar_btn.connect_clicked(move |_| {
                popover.popdown();
                match create_event(&ctx.storage, target_calendar_id, target_account_id, &edits) {
                    Ok(_) => {
                        ctx.sender.input(AppMsg::EventUpdated);
                        close_panel(&panel_root, &ctx.docks);
                    }
                    Err(err) => tracing::warn!(%err, target_calendar_id, "failed to copy event to calendar"),
                }
            });
        }
        list_box.append(&calendar_btn);
        filter_rows.push((calendar.display_name.to_lowercase(), calendar_btn));
    }

    if targets.len() > 10 {
        let filter_entry = gtk4::SearchEntry::new();
        filter_entry.add_css_class("search-field");
        filter_entry.set_placeholder_text(Some("Filter calendars"));
        {
            let filter_rows = filter_rows.clone();
            filter_entry.connect_search_changed(move |entry| {
                let query = entry.text().to_lowercase();
                for (name, row) in &filter_rows {
                    row.set_visible(query.is_empty() || name.contains(&query));
                }
            });
        }
        // Escape clears back to the unfiltered list (`stop-search`, `GtkSearchEntry`'s
        // built-in Escape binding) rather than leaving the popover's list filtered —
        // the same convention `wire_settings_nav_search`/the header search field follow.
        filter_entry.connect_stop_search(move |entry| {
            entry.set_text("");
            for (_, row) in &filter_rows {
                row.set_visible(true);
            }
        });
        root.append(&filter_entry);
        {
            let popover = popover.clone();
            popover.connect_show(move |_| {
                filter_entry.grab_focus();
            });
        }
    }

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .max_content_height(280)
        .propagate_natural_height(true)
        .child(&list_box)
        .build();
    root.append(&scroller);

    popover.set_child(Some(&root));
    button.set_popover(Some(&popover));
    button
}

/// Sends one event's details to the system print dialog as a single page of plain
/// Cairo-drawn text — same approach as the keyboard-shortcuts legend's `print_shortcuts`
/// (GTK printing draws directly on a Cairo context rather than rendering existing
/// widgets, hence the date-line formatting below duplicating `format_event_when` rather
/// than sharing it). `calendar_name` is looked up by the caller (`more_actions_button`)
/// since `EventDetail` itself only carries the calendar's id, not its display name.
fn print_event(event: &EventDetail, calendar_name: Option<&str>, date_format: DateFormat, time_format: TimeFormat, parent: &impl IsA<gtk4::Window>) {
    let title = event.title.clone();
    let when = if event.all_day {
        NaiveDate::parse_from_str(event.start.get(0..10).unwrap_or(&event.start), "%Y-%m-%d")
            .map(|d| format_popover_date(d, date_format))
            .unwrap_or_else(|_| event.start.clone())
    } else {
        match (DateTime::parse_from_rfc3339(&event.start), DateTime::parse_from_rfc3339(&event.end)) {
            (Ok(s), Ok(e)) => format!(
                "{} · {} – {}",
                format_popover_date(s.date_naive(), date_format),
                format_clock(s.time(), time_format),
                format_clock(e.time(), time_format)
            ),
            _ => format!("{} – {}", event.start, event.end),
        }
    };
    let calendar_name = calendar_name.map(str::to_string);
    let location = event.location.clone().filter(|s| !s.trim().is_empty());
    // The stored description may carry `load_description_markup`'s `<b>/<i>/<u>` tags
    // (from the rich-text toolbar) — Cairo's `show_text` has no notion of inline
    // formatting runs, so printing strips them to plain text rather than showing the
    // raw markup on the page.
    let description = event.description.as_deref().map(strip_description_markup).filter(|s| !s.trim().is_empty());

    let op = gtk4::PrintOperation::new();
    op.set_job_name(&format!("Calendarchy — {title}"));
    op.connect_begin_print(|op, _ctx| op.set_n_pages(1));
    op.connect_draw_page(move |_op, ctx, _page_nr| {
        let cr = ctx.cairo_context();
        let left_margin = 36.0;
        let text_width = ctx.width() - left_margin - 36.0;
        let mut y = 48.0;

        cr.set_source_rgb(0.0, 0.0, 0.0);
        cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Bold);
        cr.set_font_size(18.0);
        cr.move_to(left_margin, y);
        let _ = cr.show_text(&title);
        y += 30.0;

        cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Normal);
        cr.set_font_size(12.0);
        cr.move_to(left_margin, y);
        let _ = cr.show_text(&when);
        y += 24.0;

        if let Some(calendar_name) = &calendar_name {
            cr.move_to(left_margin, y);
            let _ = cr.show_text(&format!("Calendar: {calendar_name}"));
            y += 20.0;
        }
        if let Some(location) = &location {
            cr.move_to(left_margin, y);
            let _ = cr.show_text(&format!("Location: {location}"));
            y += 20.0;
        }

        if let Some(description) = &description {
            y += 10.0;
            cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Bold);
            cr.set_font_size(13.0);
            cr.move_to(left_margin, y);
            let _ = cr.show_text("Description");
            y += 20.0;

            cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Normal);
            cr.set_font_size(11.0);
            for line in wrap_text(&cr, description, text_width) {
                cr.move_to(left_margin, y);
                let _ = cr.show_text(&line);
                y += 16.0;
            }
        }
    });

    if let Err(err) = op.run(gtk4::PrintOperationAction::PrintDialog, Some(parent)) {
        tracing::warn!(%err, event_id = event.id, "failed to run print operation for event");
    }
}

/// Greedily wraps `text` into lines no wider than `max_width` points at the Cairo
/// context's currently-selected font, breaking on whitespace — `print_event`'s
/// description field needs this since Cairo's `show_text` draws one line verbatim
/// rather than wrapping the way a GTK `Label` would. Existing newlines in `text` (e.g. a
/// multi-paragraph description) are preserved as their own line breaks.
fn wrap_text(cr: &gtk4::cairo::Context, text: &str, max_width: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if current.is_empty() { word.to_string() } else { format!("{current} {word}") };
            let width = cr.text_extents(&candidate).map(|e| e.width()).unwrap_or(0.0);
            if width > max_width && !current.is_empty() {
                lines.push(current);
                current = word.to_string();
            } else {
                current = candidate;
            }
        }
        lines.push(current);
    }
    lines
}

/// `print_event`'s plain-text counterpart to `load_description_markup`: drops the
/// `<b>/<i>/<u>` tags instead of turning them into `TextBuffer` formatting, and
/// unescapes `&amp;`/`&lt;`/`&gt;` the same way. An unrecognized `<...>` token is kept
/// as literal text, same "never eaten" guarantee as the buffer loader, for descriptions
/// saved before the rich-text toolbar existed.
fn strip_description_markup(raw: &str) -> String {
    fn unescape(text: &str) -> String {
        text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
    }

    let mut out = String::new();
    let mut rest = raw;
    while let Some(lt) = rest.find('<') {
        out.push_str(&unescape(&rest[..lt]));
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            out.push_str(&unescape(&rest[lt..]));
            rest = "";
            break;
        };
        let token = &after[..gt];
        if !matches!(token, "b" | "/b" | "i" | "/i" | "u" | "/u") {
            out.push_str(&unescape(&format!("<{token}>")));
        }
        rest = &after[gt + 1..];
    }
    out.push_str(&unescape(rest));
    out
}

/// Splits one of an event's stored `start`/`end` strings (either a bare `YYYY-MM-DD`
/// for all-day events or an RFC 3339 timestamp) into a local date and time-of-day, for
/// pre-filling the edit dialog's pickers. Falls back to "now" on anything unparseable
/// rather than failing to open the dialog over one malformed field.
fn split_date_time(value: &str, all_day: bool) -> (NaiveDate, NaiveTime) {
    if all_day {
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap_or_else(|_| Local::now().date_naive());
        (date, NaiveTime::default())
    } else if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        let local = dt.with_timezone(&Local);
        (local.date_naive(), local.time())
    } else {
        let now = Local::now();
        (now.date_naive(), now.time())
    }
}

/// Combines a date and a time-of-day into a `DateTime<Local>`, resolving DST-fold
/// ambiguity by taking the earlier of the two candidates and falling back to "now" for
/// the (practically unreachable, for a spring-forward gap) case of neither matching a
/// real local instant.
fn local_datetime(date: NaiveDate, time: NaiveTime) -> DateTime<Local> {
    match Local.from_local_datetime(&NaiveDateTime::new(date, time)) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => dt,
        chrono::LocalResult::None => Local::now(),
    }
}

fn non_empty(s: String) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// A date-picker button for the edit dialog's start/end date fields, built from the
/// exact same pieces as the sidebar's mini calendar (`populate_mini_calendar` for the
/// day grid and its month/year title button, `build_quick_jump_popover` — with a
/// directly-typable year spin button — for jumping months) rather than the native
/// `gtk4::Calendar` widget, so it looks and behaves like the rest of the app instead
/// of a differently-styled system control. Its own prev/next buttons page a *local*
/// `display_month`, independent of whichever month the main calendar or the other
/// date field happens to be showing. Returns the outer button and a `Cell` holding the
/// currently-picked date, read back on Save. `dirty` is marked on every pick, same as
/// the dialog's other fields, for `install_close_guard`.
fn date_picker(initial: NaiveDate, dirty: Rc<Cell<bool>>, date_format: DateFormat) -> (gtk4::MenuButton, Rc<Cell<NaiveDate>>) {
    let today = Local::now().date_naive();
    let state = Rc::new(Cell::new(initial));
    let display_month = Rc::new(Cell::new(initial));

    let button = gtk4::MenuButton::new();
    button.add_css_class("flat");
    button.set_label(&format_picker_date(initial, date_format));

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    content.add_css_class("mini-calendar");
    content.set_margin_all(8);

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    let title_button = gtk4::MenuButton::new();
    title_button.add_css_class("flat");
    title_button.add_css_class("mini-calendar-title");
    title_button.set_hexpand(true);
    title_button.set_halign(gtk4::Align::Start);
    title_button.set_tooltip_text(Some("Jump to month/year"));
    let prev_btn = gtk4::Button::from_icon_name("go-previous-symbolic");
    prev_btn.add_css_class("flat");
    prev_btn.set_tooltip_text(Some("Previous month"));
    let next_btn = gtk4::Button::from_icon_name("go-next-symbolic");
    next_btn.add_css_class("flat");
    next_btn.set_tooltip_text(Some("Next month"));
    header.append(&title_button);
    header.append(&prev_btn);
    header.append(&next_btn);
    content.append(&header);

    let day_grid = gtk4::Grid::new();
    day_grid.add_css_class("mini-calendar-grid");
    day_grid.set_row_homogeneous(true);
    day_grid.set_column_homogeneous(true);
    day_grid.set_row_spacing(2);
    day_grid.set_column_spacing(2);
    content.append(&day_grid);

    let popover = gtk4::Popover::new();
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));

    let on_pick: Rc<dyn Fn(NaiveDate)> = {
        let state = state.clone();
        let button = button.clone();
        let popover = popover.clone();
        Rc::new(move |date: NaiveDate| {
            state.set(date);
            button.set_label(&format_picker_date(date, date_format));
            dirty.set(true);
            popover.popdown();
        })
    };

    refresh_date_picker(&day_grid, &title_button, &display_month, today, &on_pick);

    for (delta, nav_btn) in [(-1, &prev_btn), (1, &next_btn)] {
        let day_grid = day_grid.clone();
        let title_button = title_button.clone();
        let display_month = display_month.clone();
        let on_pick = on_pick.clone();
        nav_btn.connect_clicked(move |_| {
            display_month.set(shift_month(display_month.get(), delta));
            refresh_date_picker(&day_grid, &title_button, &display_month, today, &on_pick);
        });
    }

    (button, state)
}

/// Re-renders `date_picker`'s day grid for whatever month `display_month` currently
/// holds, and rebuilds its title button's quick-jump popover with a fresh
/// `on_jump_month` callback that recurses back into this same function — a named
/// function rather than a self-referential closure (which Rust can't express
/// directly: a closure can't capture an `Rc` pointing at itself before that `Rc`
/// finishes being constructed) — so jumping to a month from the quick-jump popover
/// re-renders the day grid and stays open, the same as clicking the prev/next arrows,
/// instead of running `on_pick` and closing the whole date field.
fn refresh_date_picker(
    day_grid: &gtk4::Grid,
    title_button: &gtk4::MenuButton,
    display_month: &Rc<Cell<NaiveDate>>,
    today: NaiveDate,
    on_pick: &Rc<dyn Fn(NaiveDate)>,
) {
    let on_jump_month: Rc<dyn Fn(NaiveDate)> = {
        let day_grid = day_grid.clone();
        let title_button = title_button.clone();
        let display_month = display_month.clone();
        let on_pick = on_pick.clone();
        Rc::new(move |date: NaiveDate| {
            display_month.set(date);
            refresh_date_picker(&day_grid, &title_button, &display_month, today, &on_pick);
        })
    };
    populate_mini_calendar(day_grid, title_button, display_month.get(), today, on_pick, &on_jump_month, None, &[]);
}

/// A Google-Calendar-style time field for the edit dialog's start/end time (reference
/// screenshot): a directly-editable `Entry` — type a time and press Enter or click
/// away — paired with a scrollable quarter-hour popover list that opens on focus, for
/// picking instead of typing. Both the entry and the list show times using the
/// resolved `time_format`, but *typed* input is parsed leniently regardless of that
/// display format (`parse_time_input` accepts "14:30" and "2:30pm" either way).
/// Returns the entry and a `Cell` holding the current value, read back on Save.
/// `dirty` is marked on either path, same as the dialog's other fields.
fn time_picker(initial: NaiveTime, dirty: Rc<Cell<bool>>, time_format: TimeFormat) -> (gtk4::Entry, Rc<Cell<NaiveTime>>) {
    const QUARTER_HOURS: i32 = 24 * 4;
    const POPOVER_MAX_HEIGHT: i32 = 220;

    let state = Rc::new(Cell::new(initial));

    let entry = gtk4::Entry::new();
    entry.add_css_class("time-picker-entry");
    entry.set_width_chars(8);
    gtk4::prelude::EditableExt::set_alignment(&entry, 0.5);
    entry.set_text(&format_clock(initial, time_format));

    let list_box = gtk4::ListBox::new();
    list_box.set_selection_mode(gtk4::SelectionMode::None);
    for quarter in 0..QUARTER_HOURS {
        let total_minutes = quarter * 15;
        let time = NaiveTime::from_hms_opt((total_minutes / 60) as u32, (total_minutes % 60) as u32, 0).expect("valid quarter-hour");
        let label = gtk4::Label::new(Some(&format_clock(time, time_format)));
        label.set_halign(gtk4::Align::Start);
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.set_margin_top(4);
        label.set_margin_bottom(4);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&label));
        list_box.append(&row);
    }

    let scroller = gtk4::ScrolledWindow::builder()
        .max_content_height(POPOVER_MAX_HEIGHT)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&list_box)
        .build();

    let popover = gtk4::Popover::new();
    popover.add_css_class("time-picker-popover");
    popover.set_child(Some(&scroller));
    popover.set_parent(&entry);

    let commit: Rc<dyn Fn()> = {
        let entry = entry.clone();
        let state = state.clone();
        let dirty = dirty.clone();
        Rc::new(move || {
            if let Some(parsed) = parse_time_input(&entry.text()) {
                if parsed != state.get() {
                    state.set(parsed);
                    dirty.set(true);
                }
            }
            // Reformat unconditionally, even on parse failure — snaps back to the
            // last valid value rather than leaving unparseable text in the field.
            entry.set_text(&format_clock(state.get(), time_format));
        })
    };

    {
        let state = state.clone();
        let entry = entry.clone();
        let popover = popover.clone();
        let dirty = dirty.clone();
        list_box.connect_row_activated(move |_, row| {
            let total_minutes = row.index() * 15;
            let Some(time) = NaiveTime::from_hms_opt((total_minutes / 60) as u32, (total_minutes % 60) as u32, 0) else {
                return;
            };
            state.set(time);
            entry.set_text(&format_clock(time, time_format));
            dirty.set(true);
            popover.popdown();
        });
    }

    // Scrolls the popover so the current value sits centered in the visible list
    // (rather than just barely scrolled into view, which is all `row.grab_focus()` +
    // the `ScrolledWindow`'s default focus-follow behavior would give) — computed on
    // `map`, not on the focus-in that triggers `popup()` below, since the row's
    // `measure()` and the scrolled window's adjustment bounds aren't reliable until
    // the popover has actually been shown and laid out.
    {
        let list_box = list_box.clone();
        let state = state.clone();
        let scroller = scroller.clone();
        popover.connect_map(move |_| {
            let total_minutes = state.get().hour() as i32 * 60 + state.get().minute() as i32;
            let nearest = (total_minutes / 15).clamp(0, QUARTER_HOURS - 1);
            let Some(row) = list_box.row_at_index(nearest) else {
                return;
            };
            let row_height = row.measure(gtk4::Orientation::Vertical, -1).1.max(1);
            let target_center = nearest * row_height + row_height / 2;
            let scroll_to = (target_center - POPOVER_MAX_HEIGHT / 2).max(0) as f64;
            scroller.vadjustment().set_value(scroll_to);
        });
    }

    let focus_controller = gtk4::EventControllerFocus::new();
    {
        let popover = popover.clone();
        focus_controller.connect_enter(move |_| {
            popover.popup();
        });
    }
    {
        let commit = commit.clone();
        let popover = popover.clone();
        focus_controller.connect_leave(move |_| {
            commit();
            popover.popdown();
        });
    }
    entry.add_controller(focus_controller);

    entry.connect_activate(move |_| {
        commit();
        popover.popdown();
    });

    (entry, state)
}

/// The edit dialog's "Does not repeat" ▾ control (reference screenshots): a preset
/// menu — Daily / Weekly on `<weekday>` / Monthly on the Nth `<weekday>` / Annually on
/// `<month day>` / Every weekday (Monday to Friday), each phrased against the event's
/// own start date the way Google Calendar's own dropdown is — plus a "Custom…" row
/// that opens `show_custom_recurrence_dialog`. Rebuilds its preset rows every time the
/// popover opens (rather than once, up front) so their phrasing stays correct if the
/// user changes the start date before picking a repeat option. Returns the button and
/// the dialog's live recurrence state (`None` = "does not repeat"), read back by Save.
fn build_repeat_control(
    event: &EventDetail,
    start_picked: Rc<Cell<NaiveDate>>,
    dirty: Rc<Cell<bool>>,
    date_format: DateFormat,
    parent_window: &adw::Window,
) -> (gtk4::MenuButton, Rc<RefCell<Option<Recurrence>>>) {
    let state: Rc<RefCell<Option<Recurrence>>> =
        Rc::new(RefCell::new(event.recurrence.as_deref().and_then(Recurrence::from_rrule_string)));

    let repeat_btn = gtk4::MenuButton::new();
    repeat_btn.add_css_class("flat");
    update_repeat_label(&repeat_btn, state.borrow().as_ref(), start_picked.get());

    let list_box = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    list_box.set_margin_all(6);
    list_box.set_width_request(240);

    let popover = gtk4::Popover::new();
    popover.set_child(Some(&list_box));
    repeat_btn.set_popover(Some(&popover));

    let parent_window = parent_window.clone();
    let show_repeat_btn = repeat_btn.clone();
    let show_state = state.clone();
    popover.connect_show(move |popover| {
        let repeat_btn = show_repeat_btn.clone();
        let state = show_state.clone();
        while let Some(child) = list_box.first_child() {
            list_box.remove(&child);
        }
        let start = start_picked.get();

        for (label, preset) in [
            ("Does not repeat".to_string(), None),
            (recurrence::daily().describe(start), Some(recurrence::daily())),
            (recurrence::weekly_on(start).describe(start), Some(recurrence::weekly_on(start))),
            (recurrence::monthly_on(start).describe(start), Some(recurrence::monthly_on(start))),
            (recurrence::yearly_on().describe(start), Some(recurrence::yearly_on())),
            (recurrence::every_weekday().describe(start), Some(recurrence::every_weekday())),
        ] {
            let row = menu_row_button(&label);
            let popover = popover.clone();
            let state = state.clone();
            let repeat_btn = repeat_btn.clone();
            let dirty = dirty.clone();
            row.connect_clicked(move |_| {
                *state.borrow_mut() = preset.clone();
                update_repeat_label(&repeat_btn, state.borrow().as_ref(), start);
                dirty.set(true);
                popover.popdown();
            });
            list_box.append(&row);
        }

        let custom_row = menu_row_button("Custom…");
        let popover = popover.clone();
        let state = state.clone();
        let repeat_btn = repeat_btn.clone();
        let dirty = dirty.clone();
        let parent_window = parent_window.clone();
        custom_row.connect_clicked(move |_| {
            popover.popdown();
            let initial = state.borrow().clone();
            let state = state.clone();
            let repeat_btn = repeat_btn.clone();
            let dirty = dirty.clone();
            show_custom_recurrence_dialog(&parent_window, start, date_format, initial, move |recurrence| {
                *state.borrow_mut() = Some(recurrence.clone());
                update_repeat_label(&repeat_btn, Some(&recurrence), start);
                dirty.set(true);
            });
        });
        list_box.append(&custom_row);
    });

    (repeat_btn, state)
}

fn update_repeat_label(button: &gtk4::MenuButton, recurrence: Option<&Recurrence>, start: NaiveDate) {
    let label = recurrence.map(|r| r.describe(start)).unwrap_or_else(|| "Does not repeat".to_string());
    button.set_label(&label);
}

/// `["day", "week", "month", "year"]`, pluralized together based on `n` — the "Repeat
/// every N `<unit>`" dropdown in `show_custom_recurrence_dialog` re-derives these on
/// every interval change (Google's own custom-recurrence dialog does the same: "1
/// week" becomes "2 weeks" live as the spinner changes) via `gtk4::StringList::splice`,
/// which updates the dropdown's item text in place without disturbing its selection.
fn unit_labels(n: i64) -> [String; 4] {
    let plural = n != 1;
    [
        (if plural { "days" } else { "day" }).to_string(),
        (if plural { "weeks" } else { "week" }).to_string(),
        (if plural { "months" } else { "month" }).to_string(),
        (if plural { "years" } else { "year" }).to_string(),
    ]
}

/// Opens the "Custom recurrence" dialog (reference screenshot): interval + unit, a
/// "Repeat on" day-of-week toggle row (shown only for the "week" unit, matching the
/// screenshot), and an "Ends" choice of Never / on a date / after N occurrences. Calls
/// `on_done` with the resulting `Recurrence` if the user confirms; does nothing on
/// Cancel. `initial` pre-fills every control from whatever was active on the repeat
/// dropdown when "Custom…" was clicked (a preset, a previous custom pick, or the
/// event's own pre-existing recurrence), defaulting to "every 1 week on `start`'s own
/// weekday, never ending" when there was no prior selection.
///
/// Only `Weekly`'s day set round-trips through this dialog — `Monthly`'s "Nth
/// weekday" ordinal (`monthly_on`'s preset) has no control here, matching the
/// reference screenshot, so re-opening Custom on a "Monthly on the first Tuesday"
/// selection and clicking Done resets it to a plain "same day of month" rule instead
/// of preserving the ordinal.
fn show_custom_recurrence_dialog(
    parent_window: &adw::Window,
    start: NaiveDate,
    date_format: DateFormat,
    initial: Option<Recurrence>,
    on_done: impl Fn(Recurrence) + 'static,
) {
    let interval = initial.as_ref().map(|r| r.interval.max(1)).unwrap_or(1);
    let unit_index = match initial.as_ref().map(|r| r.freq) {
        Some(recurrence::Frequency::Daily) => 0,
        Some(recurrence::Frequency::Monthly) => 2,
        Some(recurrence::Frequency::Yearly) => 3,
        _ => 1, // Weekly, and "no prior selection", both land on "week".
    };
    let selected_days: HashSet<Weekday> = match &initial {
        Some(r) if r.freq == recurrence::Frequency::Weekly && !r.by_day.is_empty() => r.by_day.iter().copied().collect(),
        _ => [start.weekday()].into_iter().collect(),
    };
    let end = initial.map(|r| r.end).unwrap_or(RecurrenceEnd::Never);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 16);

    let every_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    every_row.append(&gtk4::Label::new(Some("Repeat every")));
    let interval_spin = gtk4::SpinButton::with_range(1.0, 99.0, 1.0);
    interval_spin.set_value(interval as f64);
    interval_spin.set_width_chars(3);
    every_row.append(&interval_spin);

    let unit_model = gtk4::StringList::new(&unit_labels(interval as i64).each_ref().map(|s| s.as_str()));
    let unit_dropdown = gtk4::DropDown::new(Some(unit_model.clone()), gtk4::Expression::NONE);
    unit_dropdown.set_selected(unit_index);
    every_row.append(&unit_dropdown);
    content.append(&every_row);

    let repeat_on_label = gtk4::Label::new(Some("Repeat on"));
    repeat_on_label.add_css_class("heading");
    repeat_on_label.set_halign(gtk4::Align::Start);
    content.append(&repeat_on_label);

    let days_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    let day_order = [Weekday::Sun, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat];
    let day_letters = ["S", "M", "T", "W", "T", "F", "S"];
    let day_buttons: Vec<(Weekday, gtk4::ToggleButton)> = day_order
        .iter()
        .zip(day_letters)
        .map(|(&day, letter)| {
            let btn = gtk4::ToggleButton::with_label(letter);
            btn.add_css_class("recur-day-toggle");
            btn.set_active(selected_days.contains(&day));
            days_row.append(&btn);
            (day, btn)
        })
        .collect();
    content.append(&days_row);

    {
        let repeat_on_label = repeat_on_label.clone();
        let days_row = days_row.clone();
        let sync_day_visibility = move |selected: u32| {
            let is_weekly = selected == 1;
            repeat_on_label.set_visible(is_weekly);
            days_row.set_visible(is_weekly);
        };
        sync_day_visibility(unit_dropdown.selected());
        unit_dropdown.connect_selected_notify(move |dd| sync_day_visibility(dd.selected()));
    }
    {
        let unit_model = unit_model.clone();
        interval_spin.connect_value_changed(move |spin| {
            let labels = unit_labels(spin.value_as_int() as i64);
            unit_model.splice(0, 4, &labels.each_ref().map(|s| s.as_str()));
        });
    }

    let ends_label = gtk4::Label::new(Some("Ends"));
    ends_label.add_css_class("heading");
    ends_label.set_halign(gtk4::Align::Start);
    content.append(&ends_label);

    let never_radio = gtk4::CheckButton::with_label("Never");
    let on_radio = gtk4::CheckButton::with_label("On");
    on_radio.set_group(Some(&never_radio));
    let after_radio = gtk4::CheckButton::with_label("After");
    after_radio.set_group(Some(&never_radio));
    content.append(&never_radio);

    let (on_date_button, on_date_state) = date_picker(
        match &end {
            RecurrenceEnd::OnDate(date) => *date,
            _ => start + Duration::days(30),
        },
        Rc::new(Cell::new(false)),
        date_format,
    );
    let on_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    on_row.append(&on_radio);
    on_row.append(&on_date_button);
    content.append(&on_row);

    let after_spin = gtk4::SpinButton::with_range(1.0, 999.0, 1.0);
    after_spin.set_value(match &end {
        RecurrenceEnd::AfterCount(n) => *n as f64,
        _ => 13.0,
    });
    after_spin.set_width_chars(3);
    let after_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    after_row.append(&after_radio);
    after_row.append(&after_spin);
    after_row.append(&gtk4::Label::new(Some("occurrences")));
    content.append(&after_row);

    match &end {
        RecurrenceEnd::Never => never_radio.set_active(true),
        RecurrenceEnd::OnDate(_) => on_radio.set_active(true),
        RecurrenceEnd::AfterCount(_) => after_radio.set_active(true),
    }

    let sync_end_sensitivity: Rc<dyn Fn()> = {
        let on_radio = on_radio.clone();
        let after_radio = after_radio.clone();
        let on_date_button = on_date_button.clone();
        let after_spin = after_spin.clone();
        Rc::new(move || {
            on_date_button.set_sensitive(on_radio.is_active());
            after_spin.set_sensitive(after_radio.is_active());
        })
    };
    sync_end_sensitivity();
    for radio in [&never_radio, &on_radio, &after_radio] {
        let sync_end_sensitivity = sync_end_sensitivity.clone();
        radio.connect_toggled(move |_| sync_end_sensitivity());
    }

    let dialog = adw::AlertDialog::new(Some("Custom recurrence"), None);
    dialog.set_extra_child(Some(&content));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("done", "Done");
    dialog.set_response_appearance("done", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("done"));
    dialog.set_close_response("cancel");

    dialog.choose(Some(parent_window), gtk4::gio::Cancellable::NONE, move |response| {
        if response != "done" {
            return;
        }
        let freq = match unit_dropdown.selected() {
            0 => recurrence::Frequency::Daily,
            2 => recurrence::Frequency::Monthly,
            3 => recurrence::Frequency::Yearly,
            _ => recurrence::Frequency::Weekly,
        };
        let mut by_day: Vec<Weekday> =
            day_buttons.iter().filter(|(_, btn)| btn.is_active()).map(|(day, _)| *day).collect();
        if freq == recurrence::Frequency::Weekly {
            if by_day.is_empty() {
                by_day.push(start.weekday());
            }
        } else {
            by_day.clear();
        }
        let end = if after_radio.is_active() {
            RecurrenceEnd::AfterCount((after_spin.value_as_int().max(1)) as u32)
        } else if on_radio.is_active() {
            RecurrenceEnd::OnDate(on_date_state.get())
        } else {
            RecurrenceEnd::Never
        };

        on_done(Recurrence {
            freq,
            interval: interval_spin.value_as_int().max(1) as u32,
            by_day,
            monthly_ordinal: None,
            end,
        });
    });
}

/// Parses freely-typed time input for `time_picker`'s entry — independent of the
/// field's display `TimeFormat`, so either "14:30" or "2:30pm" works regardless of
/// which format is currently shown. Returns `None` on anything it can't make sense
/// of, so the caller can fall back to the last valid value instead of accepting
/// garbage.
fn parse_time_input(text: &str) -> Option<NaiveTime> {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return None;
    }

    let (digits_part, meridiem) = if let Some(stripped) = text.strip_suffix("am") {
        (stripped.trim(), Some(false))
    } else if let Some(stripped) = text.strip_suffix("pm") {
        (stripped.trim(), Some(true))
    } else {
        (text.as_str(), None)
    };

    let (hour_str, minute_str) = digits_part
        .split_once(':')
        .or_else(|| digits_part.split_once('.'))
        .unwrap_or((digits_part, "0"));

    let mut hour: u32 = hour_str.trim().parse().ok()?;
    let minute: u32 = minute_str.trim().parse().ok()?;
    if minute > 59 {
        return None;
    }

    if let Some(is_pm) = meridiem {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if is_pm {
            hour += 12;
        }
    } else if hour > 23 {
        return None;
    }

    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// The small circular color-swatch button next to the edit dialog's calendar dropdown
/// (reference screenshot): lets one event override its calendar's color rather than
/// always inheriting it, mirroring Google Calendar's own per-event color picker.
/// Reuses `CALENDAR_COLOR_PALETTE` and the `calendar_customizer_button` swatch-grid
/// convention, prefixed with a "Calendar color" option (`None`) so most events don't
/// need to touch this at all. Returns the button and a `RefCell` holding the current
/// choice, read back on Save. `dirty` is marked on every pick, same as the dialog's
/// other fields.
fn event_color_picker(initial: Option<String>, dirty: Rc<Cell<bool>>) -> (gtk4::MenuButton, Rc<RefCell<Option<String>>>) {
    let state = Rc::new(RefCell::new(initial.clone()));

    // A plain `MenuButton` with no icon/label/child falls back to rendering just its
    // dropdown arrow (there's nothing else to show), so the swatch has to be an
    // explicit child widget — `color_dot`, distinct from the per-palette-color
    // `swatch` buttons built inside the popover below — and
    // `apply_event_color_button_style` re-colors *this*, not the button itself.
    // Giving `set_child` a custom widget also suppresses the arrow by default
    // (`always-show-arrow` defaults to false), leaving a clean dot.
    let color_dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    color_dot.add_css_class("event-color-swatch");
    apply_event_color_button_style(&color_dot, initial.as_deref());

    let button = gtk4::MenuButton::new();
    button.add_css_class("flat");
    button.add_css_class("event-color-button");
    button.set_valign(gtk4::Align::Center);
    button.set_tooltip_text(Some("Event color"));
    button.set_child(Some(&color_dot));

    let popover = gtk4::Popover::new();
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    root.set_margin_all(6);
    root.set_width_request(180);

    let default_btn = menu_row_button("Calendar color");
    {
        let state = state.clone();
        let color_dot = color_dot.clone();
        let popover = popover.clone();
        let dirty = dirty.clone();
        default_btn.connect_clicked(move |_| {
            *state.borrow_mut() = None;
            apply_event_color_button_style(&color_dot, None);
            dirty.set(true);
            popover.popdown();
        });
    }
    root.append(&default_btn);
    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

    let colors_grid = gtk4::Grid::new();
    colors_grid.set_row_spacing(6);
    colors_grid.set_column_spacing(6);
    colors_grid.set_margin_top(8);
    colors_grid.set_halign(gtk4::Align::Center);

    for (index, &(color, name)) in CALENDAR_COLOR_PALETTE.iter().enumerate() {
        let swatch = gtk4::Button::new();
        swatch.add_css_class("flat");
        swatch.add_css_class("circular");
        swatch.add_css_class("color-swatch");
        swatch.add_css_class(&css_class_for_color(color));
        swatch.set_tooltip_text(Some(name));
        if initial.as_deref() == Some(color) {
            swatch.add_css_class("color-swatch-selected");
            swatch.set_icon_name("object-select-symbolic");
        }

        let state = state.clone();
        let color_dot = color_dot.clone();
        let popover = popover.clone();
        let dirty = dirty.clone();
        let color_owned = color.to_string();
        swatch.connect_clicked(move |_| {
            *state.borrow_mut() = Some(color_owned.clone());
            apply_event_color_button_style(&color_dot, Some(&color_owned));
            dirty.set(true);
            popover.popdown();
        });

        colors_grid.attach(&swatch, (index % 6) as i32, (index / 6) as i32, 1, 1);
    }
    root.append(&colors_grid);

    popover.set_child(Some(&root));
    button.set_popover(Some(&popover));

    (button, state)
}

/// Re-styles the event-color button's `color_dot` child for its current choice:
/// filled with that color's `dot-<hex>` class (registered up front by
/// `load_palette_color_css`, so it renders even for a palette color no calendar has
/// used yet) when overridden, or a plain outline when `None` ("use the calendar's
/// color"). Strips any previously-applied `dot-*` class first so repeated picks don't
/// leave stale classes stacked on the dot — CSS classes, unlike a widget property,
/// don't get replaced by simply adding a new one.
fn apply_event_color_button_style(color_dot: &gtk4::Box, color: Option<&str>) {
    for class in color_dot.css_classes() {
        if class.starts_with("dot-") {
            color_dot.remove_css_class(&class);
        }
    }
    color_dot.remove_css_class("event-color-default");
    match color {
        Some(color) => color_dot.add_css_class(&css_class_for_color(color)),
        None => color_dot.add_css_class("event-color-default"),
    }
}

/// The unit choices in a reminder row's "10 [minutes ▾]" dropdown, each paired with
/// how many minutes one unit is — used to convert the row's quantity+unit back into
/// the single `minutes` value `EventReminder` (and Google's API) actually stores.
const REMINDER_UNITS: [(&str, i64); 4] = [("minutes", 1), ("hours", 60), ("days", 24 * 60), ("weeks", 7 * 24 * 60)];

/// The unit choices for the Preferences "Default snooze duration" setting and the
/// floating dialog's Snooze button label — deliberately narrower than
/// `REMINDER_UNITS` (no "weeks"; a snooze that long isn't a realistic use case), and
/// abbreviated since both consumers show it inline next to a number.
pub(crate) const SNOOZE_UNITS: [(&str, i64); 3] = [("mins", 1), ("hrs", 60), ("days", 24 * 60)];

/// Google Calendar caps an event at 5 reminder overrides; the edit dialog's "Add
/// notification" button disables itself at the same limit rather than accepting rows
/// a real sync could never push.
const MAX_REMINDERS: usize = 5;

/// Splits a total lead time in minutes into a `(quantity, unit index into `units`)`
/// pair for display — the largest unit that divides evenly, so e.g. 10080 against
/// `REMINDER_UNITS` shows as "1 week" rather than "10080 minutes". `0` always displays
/// as index `0` (there's no such thing as "0 weeks" as a distinct concept), which the
/// `total_minutes != 0` guard on every non-base-unit iteration ensures.
pub(crate) fn minutes_to_quantity_unit(total_minutes: i64, units: &[(&str, i64)]) -> (i64, u32) {
    for (index, &(_, multiplier)) in units.iter().enumerate().rev() {
        if total_minutes != 0 && total_minutes % multiplier == 0 {
            return (total_minutes / multiplier, index as u32);
        }
    }
    (total_minutes, 0)
}

/// A reminder's lead time as a read-only sentence for `show_event_popover`, e.g.
/// "Notification — 10 minutes before" or "Email — 1 day before" — reuses
/// `minutes_to_quantity_unit`/`REMINDER_UNITS` (the same pair the edit dialog's
/// "[10] [minutes ▾]" row already converts a stored `minutes` value through) rather
/// than re-deriving the unit breakdown.
fn format_reminder_lead_time(reminder: &EventReminder) -> String {
    let (quantity, unit_index) = minutes_to_quantity_unit(reminder.minutes, &REMINDER_UNITS);
    let unit = REMINDER_UNITS[unit_index as usize].0;
    let unit = if quantity == 1 { unit.trim_end_matches('s') } else { unit };
    let method = match reminder.method {
        ReminderMethod::Popup => "Notification",
        ReminderMethod::Email => "Email",
    };
    format!("{method} — {quantity} {unit} before")
}

/// One reminder row's live widgets, kept around (in the edit dialog's
/// `Rc<RefCell<Vec<ReminderRow>>>`) so Save can read every row's current values and so
/// a row's own "✕" button can find and remove itself.
#[derive(Clone)]
struct ReminderRow {
    root: gtk4::Box,
    method_dropdown: gtk4::DropDown,
    quantity_spin: gtk4::SpinButton,
    unit_dropdown: gtk4::DropDown,
}

/// Builds one "[Notification ▾] [10] [minutes ▾] [✕]" reminder row (reference
/// screenshots), appends it to `container`, and registers it in `rows` — shared by
/// the edit dialog's initial pre-fill (one call per existing `EventReminder`) and its
/// "Add notification" button (one call with a fresh default). The method dropdown's
/// two entries are Google's own reminder methods, `["Notification", "Email"]` in that
/// order — index 0 is `popup` (Google's UI also labels it "Notification"), index 1 is
/// `email`. Removing the row re-enables `add_notif_btn` unconditionally, since
/// dropping any row always leaves the list below `MAX_REMINDERS` again.
fn add_reminder_row(
    container: &gtk4::Box,
    rows: &Rc<RefCell<Vec<ReminderRow>>>,
    add_notif_btn: &gtk4::Button,
    dirty: &Rc<Cell<bool>>,
    reminder: &EventReminder,
) {
    let (quantity, unit_index) = minutes_to_quantity_unit(reminder.minutes, &REMINDER_UNITS);

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    row.add_css_class("reminder-row");

    let method_dropdown = gtk4::DropDown::from_strings(&["Notification", "Email"]);
    method_dropdown.set_selected(match reminder.method {
        ReminderMethod::Popup => 0,
        ReminderMethod::Email => 1,
    });

    let quantity_spin = gtk4::SpinButton::with_range(0.0, 999.0, 1.0);
    quantity_spin.set_value(quantity as f64);
    quantity_spin.set_width_chars(3);

    let unit_names: Vec<&str> = REMINDER_UNITS.iter().map(|(name, _)| *name).collect();
    let unit_dropdown = gtk4::DropDown::from_strings(&unit_names);
    unit_dropdown.set_selected(unit_index);

    for dropdown in [&method_dropdown, &unit_dropdown] {
        let dirty = dirty.clone();
        dropdown.connect_selected_notify(move |_| dirty.set(true));
    }
    {
        let dirty = dirty.clone();
        quantity_spin.connect_value_changed(move |_| dirty.set(true));
    }

    let remove_btn = gtk4::Button::from_icon_name("window-close-symbolic");
    remove_btn.add_css_class("flat");
    remove_btn.add_css_class("circular");
    remove_btn.set_tooltip_text(Some("Remove notification"));
    {
        let container = container.clone();
        let rows = rows.clone();
        let add_notif_btn = add_notif_btn.clone();
        let dirty = dirty.clone();
        let row_for_removal = row.clone();
        remove_btn.connect_clicked(move |_| {
            container.remove(&row_for_removal);
            rows.borrow_mut().retain(|r| r.root != row_for_removal);
            add_notif_btn.set_sensitive(true);
            dirty.set(true);
        });
    }

    row.append(&method_dropdown);
    row.append(&quantity_spin);
    row.append(&unit_dropdown);
    row.append(&remove_btn);
    container.append(&row);

    rows.borrow_mut().push(ReminderRow {
        root: row,
        method_dropdown,
        quantity_spin,
        unit_dropdown,
    });
}

/// Binds Escape to closing the edit dialog via `Window::close`, so it runs through
/// the same `close-request` dirty guard as the header bar's close button rather than
/// discarding an in-progress edit unconditionally. `Global` scope (mirroring
/// `install_search_shortcut`) makes it fire no matter which field currently has
/// keyboard focus — the title entry, the description text view, etc.
fn install_escape_to_close(window: &adw::Window) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    let window_for_shortcut = window.clone();
    controller.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("Escape"),
        Some(gtk4::CallbackAction::new(move |_widget, _args| {
            window_for_shortcut.close();
            gtk4::glib::Propagation::Stop
        })),
    ));

    window.add_controller(controller);
}

/// One `<icon> <widget>` row in the edit dialog — the same icon-prefixed field shape
/// Google Calendar's own event editor uses for location/description/calendar/
/// notifications, generalized from `detail_row` (which only ever holds a text label)
/// to hold any interactive widget. `tooltip` names what the row's icon stands for
/// (e.g. "Location", "Busy/visibility") — several of these symbolic icons aren't
/// self-explanatory on their own (`view-reveal-symbolic` for busy/visibility, say),
/// same reasoning as `event_badge_row`'s chip icons.
fn field_row(icon_name: &str, tooltip: &str, widget: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);

    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.add_css_class("dim-label");
    icon.set_valign(gtk4::Align::Start);
    icon.set_tooltip_text(Some(tooltip));
    row.append(&icon);

    widget.set_hexpand(true);
    row.append(widget);

    row
}

/// The three inline styles the description field's formatting toolbar supports,
/// registered once per dialog on the description `TextBuffer`'s tag table.
/// Serialized to/from the stored `description` string as minimal, hand-rolled
/// pseudo-markup (`<b>`/`<i>`/`<u>`) — deliberately *not* `TextBuffer::insert_markup`
/// (which parses real Pango markup) for loading, since that applies formatting via
/// its own anonymous tags rather than these named ones, and `serialize_description`
/// below identifies formatted runs by checking for these *exact* tag objects.
#[derive(Clone)]
struct DescriptionTags {
    bold: gtk4::TextTag,
    italic: gtk4::TextTag,
    underline: gtk4::TextTag,
}

fn install_description_tags(buffer: &gtk4::TextBuffer) -> DescriptionTags {
    let bold = gtk4::TextTag::builder().name("bold").weight(700).build();
    let italic = gtk4::TextTag::builder().name("italic").style(gtk4::pango::Style::Italic).build();
    let underline = gtk4::TextTag::builder().name("underline").underline(gtk4::pango::Underline::Single).build();
    let table = buffer.tag_table();
    table.add(&bold);
    table.add(&italic);
    table.add(&underline);
    DescriptionTags { bold, italic, underline }
}

/// Loads a stored description into `buffer`, re-applying `<b>`/`<i>`/`<u>` runs as
/// real formatting via `tags`. Hand-rolled rather than a general markup/HTML parser —
/// it only ever needs to understand its own three tags — but still has to cope with
/// *pre-existing* plain-text descriptions (every event saved before this feature
/// shipped) that might contain a literal `<`/`&`/`>`: an unrecognized `<...>` token is
/// re-inserted as literal text instead of silently eaten, so old data never vanishes.
fn load_description_markup(buffer: &gtk4::TextBuffer, raw: &str, tags: &DescriptionTags) {
    buffer.set_text("");
    let mut iter = buffer.end_iter();
    let (mut bold_on, mut italic_on, mut underline_on) = (false, false, false);
    let mut rest = raw;

    while let Some(lt) = rest.find('<') {
        if lt > 0 {
            insert_description_run(buffer, &mut iter, &rest[..lt], bold_on, italic_on, underline_on, tags);
        }
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else {
            // Unterminated '<' — treat the remainder as literal text.
            insert_description_run(buffer, &mut iter, &rest[lt..], bold_on, italic_on, underline_on, tags);
            rest = "";
            break;
        };
        let token = &after[..gt];
        match token {
            "b" => bold_on = true,
            "/b" => bold_on = false,
            "i" => italic_on = true,
            "/i" => italic_on = false,
            "u" => underline_on = true,
            "/u" => underline_on = false,
            _ => {
                let literal = format!("<{token}>");
                insert_description_run(buffer, &mut iter, &literal, bold_on, italic_on, underline_on, tags);
            }
        }
        rest = &after[gt + 1..];
    }
    if !rest.is_empty() {
        insert_description_run(buffer, &mut iter, rest, bold_on, italic_on, underline_on, tags);
    }
}

fn insert_description_run(
    buffer: &gtk4::TextBuffer,
    iter: &mut gtk4::TextIter,
    text: &str,
    bold: bool,
    italic: bool,
    underline: bool,
    tags: &DescriptionTags,
) {
    let unescaped = text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
    if unescaped.is_empty() {
        return;
    }
    let mut active: Vec<&gtk4::TextTag> = Vec::new();
    if bold {
        active.push(&tags.bold);
    }
    if italic {
        active.push(&tags.italic);
    }
    if underline {
        active.push(&tags.underline);
    }
    buffer.insert_with_tags(iter, &unescaped, &active);
}

/// The inverse of `load_description_markup`: walks the buffer's tag-toggle
/// boundaries, wrapping each run in whichever of `<b>`/`<i>`/`<u>` are active over it
/// (self-contained per run rather than merged across runs — more bytes stored for an
/// adjacent pair of identically-formatted runs, but avoids having to track
/// cross-segment nesting state, and round-trips through `load_description_markup`
/// identically either way).
fn serialize_description(buffer: &gtk4::TextBuffer, tags: &DescriptionTags) -> String {
    let end = buffer.end_iter();
    let mut iter = buffer.start_iter();
    if iter == end {
        return String::new();
    }

    let mut out = String::new();
    loop {
        let mut segment_end = iter;
        if !segment_end.forward_to_tag_toggle(None::<&gtk4::TextTag>) || segment_end <= iter {
            segment_end = end;
        }
        let raw_text = buffer.text(&iter, &segment_end, false).to_string();
        if !raw_text.is_empty() {
            let mut wrapped = raw_text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            if iter.has_tag(&tags.underline) {
                wrapped = format!("<u>{wrapped}</u>");
            }
            if iter.has_tag(&tags.italic) {
                wrapped = format!("<i>{wrapped}</i>");
            }
            if iter.has_tag(&tags.bold) {
                wrapped = format!("<b>{wrapped}</b>");
            }
            out.push_str(&wrapped);
        }
        if segment_end >= end {
            break;
        }
        iter = segment_end;
    }
    out
}

/// Toggles `tag` over the current text selection: removes it if the whole selection
/// already has it, applies it to the whole selection otherwise — the usual "apply
/// unless already uniform" convention a Bold/Italic/Underline button follows. A no-op
/// with nothing selected (these buttons format already-typed text; there's no
/// "start typing in bold" input mode to toggle instead).
fn toggle_tag_on_selection(buffer: &gtk4::TextBuffer, tag: &gtk4::TextTag) {
    let Some((start, end)) = buffer.selection_bounds() else {
        return;
    };
    let mut fully_tagged = true;
    let mut cursor = start;
    while cursor < end {
        if !cursor.has_tag(tag) {
            fully_tagged = false;
            break;
        }
        if !cursor.forward_char() {
            break;
        }
    }
    if fully_tagged {
        buffer.remove_tag(tag, &start, &end);
    } else {
        buffer.apply_tag(tag, &start, &end);
    }
}

/// Prefixes every line the current selection touches (or just the cursor's line, with
/// nothing selected) with "• " or "1. "/"2. "/… — plain text, not a `TextTag`, so
/// list formatting persists through `serialize_description` for free. Replaces
/// (rather than stacking onto) any prefix a line already has, so re-clicking or
/// switching from bullets to numbers doesn't double up.
fn apply_line_prefix(buffer: &gtk4::TextBuffer, numbered: bool) {
    let (sel_start, sel_end) = buffer.selection_bounds().unwrap_or_else(|| {
        let insert = buffer.iter_at_mark(&buffer.get_insert());
        (insert, insert)
    });
    let (first_line, last_line) = (sel_start.line(), sel_end.line());

    let mut number = 1;
    for line in first_line..=last_line {
        let Some(mut line_start) = buffer.iter_at_line(line) else { continue };
        let mut line_end = line_start;
        line_end.forward_to_line_end();
        let existing = buffer.text(&line_start, &line_end, false).to_string();
        let remainder = strip_known_line_prefix(&existing);

        buffer.delete(&mut line_start, &mut line_end);
        let prefix = if numbered {
            let n = number;
            number += 1;
            format!("{n}. ")
        } else {
            "• ".to_string()
        };
        buffer.insert(&mut line_start, &format!("{prefix}{remainder}"));
    }
}

fn strip_known_line_prefix(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix("• ") {
        return rest;
    }
    if let Some((digits, rest)) = line.split_once(". ") {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return rest;
        }
    }
    line
}

/// Builds the description field's formatting toolbar (reference screenshots): Bold,
/// Italic, Underline, bulleted/numbered list, Insert link, and Clear formatting,
/// followed by a Voxtype dictation button when `voxtype_available` (§ below) —
/// omitted entirely rather than shown disabled, since "not installed on this system"
/// is a different situation from this app's usual "not built yet" affordances.
fn build_description_toolbar(description_view: &gtk4::TextView, tags: &DescriptionTags, dirty: &Rc<Cell<bool>>) -> gtk4::Box {
    let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    toolbar.add_css_class("description-toolbar");

    let buffer = description_view.buffer();

    for (icon_name, tooltip, tag) in [
        ("format-text-bold-symbolic", "Bold", &tags.bold),
        ("format-text-italic-symbolic", "Italic", &tags.italic),
        ("format-text-underline-symbolic", "Underline", &tags.underline),
    ] {
        let btn = gtk4::Button::from_icon_name(icon_name);
        btn.add_css_class("flat");
        btn.set_tooltip_text(Some(tooltip));
        let buffer = buffer.clone();
        let tag = tag.clone();
        let dirty = dirty.clone();
        btn.connect_clicked(move |_| {
            toggle_tag_on_selection(&buffer, &tag);
            dirty.set(true);
        });
        toolbar.append(&btn);
    }

    for (icon_name, tooltip, numbered) in [
        ("view-list-bullet-symbolic", "Bulleted list", false),
        ("view-list-ordered-symbolic", "Numbered list", true),
    ] {
        let btn = gtk4::Button::from_icon_name(icon_name);
        btn.add_css_class("flat");
        btn.set_tooltip_text(Some(tooltip));
        let buffer = buffer.clone();
        let dirty = dirty.clone();
        btn.connect_clicked(move |_| {
            apply_line_prefix(&buffer, numbered);
            dirty.set(true);
        });
        toolbar.append(&btn);
    }

    install_link_button(&toolbar, description_view, dirty);

    let clear_btn = gtk4::Button::from_icon_name("edit-clear-all-symbolic");
    clear_btn.add_css_class("flat");
    clear_btn.set_tooltip_text(Some("Clear formatting"));
    {
        let buffer = buffer.clone();
        let tags = tags.clone();
        let dirty = dirty.clone();
        clear_btn.connect_clicked(move |_| {
            if let Some((start, end)) = buffer.selection_bounds() {
                buffer.remove_tag(&tags.bold, &start, &end);
                buffer.remove_tag(&tags.italic, &start, &end);
                buffer.remove_tag(&tags.underline, &start, &end);
                dirty.set(true);
            }
        });
    }
    toolbar.append(&clear_btn);

    install_mic_button(&toolbar, description_view);

    toolbar
}

/// "Insert link" (reference screenshot): a small popover prompting for a URL, which
/// replaces the current selection (if any) with the typed URL as plain text. Simpler
/// than a real clickable hyperlink — no new `TextTag` per link, no href bookkeeping —
/// but still directly useful, and the plain-text result needs no serialization
/// support beyond what already exists for the rest of the description.
fn install_link_button(toolbar: &gtk4::Box, description_view: &gtk4::TextView, dirty: &Rc<Cell<bool>>) {
    let link_btn = gtk4::MenuButton::new();
    link_btn.set_icon_name("insert-link-symbolic");
    link_btn.add_css_class("flat");
    link_btn.set_tooltip_text(Some("Insert link"));

    let popover = gtk4::Popover::new();
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    row.set_margin_all(8);
    let url_entry = gtk4::Entry::new();
    url_entry.set_placeholder_text(Some("https://example.com"));
    url_entry.set_width_chars(24);
    let insert_btn = gtk4::Button::with_label("Insert");
    insert_btn.add_css_class("suggested-action");
    row.append(&url_entry);
    row.append(&insert_btn);
    popover.set_child(Some(&row));
    link_btn.set_popover(Some(&popover));

    let do_insert: Rc<dyn Fn()> = {
        let description_view = description_view.clone();
        let url_entry = url_entry.clone();
        let popover = popover.clone();
        let dirty = dirty.clone();
        Rc::new(move || {
            let url = url_entry.text().trim().to_string();
            if url.is_empty() {
                return;
            }
            let buffer = description_view.buffer();
            buffer.delete_selection(true, true);
            let mut iter = buffer.iter_at_mark(&buffer.get_insert());
            buffer.insert(&mut iter, &url);
            dirty.set(true);
            url_entry.set_text("");
            popover.popdown();
        })
    };
    {
        let do_insert = do_insert.clone();
        insert_btn.connect_clicked(move |_| do_insert());
    }
    url_entry.connect_activate(move |_| do_insert());

    toolbar.append(&link_btn);
}

/// Whether Omarchy's Voxtype dictation (`voxtype-bin`) is installed — checked by
/// searching `$PATH` directly rather than shelling out, since this runs once per
/// dialog open. Mirrors what `omarchy-cmd-present voxtype` checks (`command -v`).
fn voxtype_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("voxtype").is_file()))
        .unwrap_or(false)
}

/// Adds a dictation toggle button when Voxtype (§ `voxtype_available`) is set up:
/// pressed spawns `voxtype record start` and focuses the description field (so
/// Voxtype's own `wtype`-simulated typing lands there rather than wherever focus
/// happened to be), unpressed spawns `voxtype record stop`. A `ToggleButton` rather
/// than a press/release `GestureClick` — attaching a second click gesture directly to
/// a `Button` fights with the button's own internal one; toggling instead also gives
/// a persistent pressed-look while recording, a reasonable proxy for Voxtype's actual
/// state without polling `voxtype status --follow` for it.
fn install_mic_button(toolbar: &gtk4::Box, description_view: &gtk4::TextView) {
    if !voxtype_available() {
        return;
    }

    let mic_btn = gtk4::ToggleButton::new();
    mic_btn.set_icon_name("audio-input-microphone-symbolic");
    mic_btn.add_css_class("flat");
    mic_btn.set_tooltip_text(Some("Dictate (Voxtype)"));

    let description_view = description_view.clone();
    mic_btn.connect_toggled(move |btn| {
        let action = if btn.is_active() {
            description_view.grab_focus();
            "start"
        } else {
            "stop"
        };
        if let Err(err) = std::process::Command::new("voxtype").args(["record", action]).spawn() {
            tracing::warn!(%err, action, "failed to control voxtype dictation");
        }
    });

    toolbar.append(&mic_btn);
}

/// One `<icon> <text>` metadata row in the event-detail popover (location,
/// description, calendar name) — same shape as Google Calendar's own detail card.
fn detail_row(icon_name: &str, text: &str) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row.add_css_class("event-popover-detail-row");

    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.add_css_class("dim-label");
    icon.set_valign(gtk4::Align::Start);
    row.append(&icon);

    let label = gtk4::Label::new(Some(text));
    label.set_halign(gtk4::Align::Start);
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_hexpand(true);
    row.append(&label);

    row
}

/// Rendered guest rows before the rest collapse into a "N more" line — same idea as
/// `compute_max_visible_events`'s per-day cap in the month grid, just a fixed number
/// here rather than measured, since the popover's width (`root.set_width_request(320)`
/// in `show_event_popover`) doesn't vary the way a day cell's height does.
const MAX_VISIBLE_GUESTS: usize = 8;

/// The event-detail popover's "Guests (N)" section — a header plus one `guest_row` per
/// attendee (already DB-ordered organizer-first by `event_attendees`'s `load_attendees`
/// query), capped at `MAX_VISIBLE_GUESTS` with a trailing "N more" line past that,
/// reusing the same `.more-label` styling the month grid's day cells use for their own
/// "N more" overflow. Only called when `attendees` is non-empty — `show_event_popover`
/// skips this section entirely otherwise, matching how it already skips the
/// location/description rows when those are absent.
fn guest_list_section(attendees: &[EventAttendeeInfo], organizer_email: Option<&str>) -> gtk4::Box {
    let section = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    section.add_css_class("event-popover-guest-section");

    let header = gtk4::Label::new(Some(&format!("Guests ({})", attendees.len())));
    header.add_css_class("event-popover-guest-header");
    header.set_halign(gtk4::Align::Start);
    section.append(&header);

    for attendee in attendees.iter().take(MAX_VISIBLE_GUESTS) {
        section.append(&guest_row(attendee, organizer_email));
    }

    if attendees.len() > MAX_VISIBLE_GUESTS {
        let more = gtk4::Label::new(Some(&format!("{} more", attendees.len() - MAX_VISIBLE_GUESTS)));
        more.add_css_class("more-label");
        more.set_halign(gtk4::Align::Start);
        more.set_margin_start(20); // aligns with `.event-popover-guest-row`'s own indent
        section.append(&more);
    }

    section
}

/// One attendee row: a small RSVP-colored dot (`guest_status_css_class`, the same
/// colored-dot idiom `.event-dot`/`.event-popover-dot` already use elsewhere in this
/// file, just with a status palette instead of a calendar-color one) plus their display
/// name (falling back to email when no display name is set), tagged " (You)" for the
/// signed-in account's own row or " (Organizer)" for whichever attendee's email matches
/// `EventDetail::organizer_email` — `EventAttendeeInfo` itself has no `is_organizer`
/// field (only the `event_attendees` table does, used solely to sort this list
/// organizer-first — see `load_attendees`), so comparing against the event's own
/// `organizer_email` is how this file surfaces that without touching the query layer.
fn guest_row(attendee: &EventAttendeeInfo, organizer_email: Option<&str>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.add_css_class("event-popover-guest-row");

    let status_dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    status_dot.add_css_class("event-popover-guest-status");
    status_dot.add_css_class(guest_status_css_class(attendee.response_status));
    status_dot.set_valign(gtk4::Align::Center);
    status_dot.set_tooltip_text(Some(guest_status_label(attendee.response_status)));
    row.append(&status_dot);

    let mut name =
        attendee.display_name.as_deref().filter(|n| !n.trim().is_empty()).unwrap_or(&attendee.email).to_string();
    if attendee.is_self {
        name.push_str(" (You)");
    } else if organizer_email.map_or(false, |organizer| attendee.email.eq_ignore_ascii_case(organizer)) {
        name.push_str(" (Organizer)");
    }

    let label = gtk4::Label::new(Some(&name));
    label.set_halign(gtk4::Align::Start);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row.append(&label);

    row
}

/// CSS class for a guest row's RSVP-status dot — a distinct color per
/// `AttendeeResponseStatus`, mirroring Google Calendar's own guest-list color coding.
fn guest_status_css_class(status: AttendeeResponseStatus) -> &'static str {
    match status {
        AttendeeResponseStatus::Accepted => "event-popover-guest-status-accepted",
        AttendeeResponseStatus::Declined => "event-popover-guest-status-declined",
        AttendeeResponseStatus::Tentative => "event-popover-guest-status-tentative",
        AttendeeResponseStatus::NeedsAction => "event-popover-guest-status-needs-action",
    }
}

/// Human-readable RSVP status for `guest_row`'s status-dot tooltip — a bare colored
/// dot conveys nothing on its own without hovering it, same reasoning as every other
/// bare-icon tooltip in this file.
fn guest_status_label(status: AttendeeResponseStatus) -> &'static str {
    match status {
        AttendeeResponseStatus::Accepted => "Accepted",
        AttendeeResponseStatus::Declined => "Declined",
        AttendeeResponseStatus::Tentative => "Maybe",
        AttendeeResponseStatus::NeedsAction => "Awaiting response",
    }
}

/// Formats an event's date/time for the detail popover, e.g. "Monday, August 31 ·
/// 11:05 – 12:58" for a timed event or "Monday, August 31" for an all-day one —
/// matching the Google Calendar PWA's own event-card date line, honoring
/// DESIGN_SPEC.md §12's Date format / Time format overrides.
fn format_event_when(event: &DisplayEvent, date_format: DateFormat, time_format: TimeFormat) -> String {
    if event.all_day {
        return NaiveDate::parse_from_str(event.start_date(), "%Y-%m-%d")
            .map(|d| format_popover_date(d, date_format))
            .unwrap_or_else(|_| event.start.clone());
    }

    let start = DateTime::parse_from_rfc3339(&event.start).ok();
    let end = DateTime::parse_from_rfc3339(&event.end).ok();
    match (start, end) {
        (Some(s), Some(e)) => format!(
            "{} · {} – {}",
            format_popover_date(s.date_naive(), date_format),
            format_clock(s.time(), time_format),
            format_clock(e.time(), time_format)
        ),
        _ => format!("{} – {}", event.start, event.end),
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("valid year/month");
    let first_of_this = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month");
    (first_of_next - first_of_this).num_days() as u32
}

/// One key/description pair in the keyboard-shortcuts legend, grouped under a
/// section heading.
struct ShortcutGroup {
    title: &'static str,
    items: &'static [(&'static str, &'static str)],
}

/// The bindings DESIGN_SPEC.md §10 defines for Calendarchy's keyboard navigation —
/// the header bar's `?` button (and, per §12, the `?` key itself) opens a read-only
/// cheat sheet listing exactly this set. Not every entry is wired up yet
/// (Schedule/Week/Year views and event creation are later roadmap phases, §19 —
/// D/M/X are live, see `install_view_shortcuts`), but the legend documents the full
/// intended scheme rather than only what's implemented so far — the same "coming
/// soon" treatment the sidebar and header bar already give other not-yet-built
/// features.
const SHORTCUT_GROUPS: &[ShortcutGroup] = &[
    ShortcutGroup {
        title: "Navigation",
        items: &[
            ("T", "Jump to today"),
            ("← / →", "Previous / next month"),
            ("↑ ↓ ← → , h j k l", "Move between days or weeks"),
        ],
    },
    ShortcutGroup {
        title: "Views",
        items: &[
            ("D", "Day view"),
            ("W", "Week view"),
            ("M", "Month view"),
            ("Y", "Year view"),
            ("A", "Schedule view"),
            ("X", "5-day view"),
        ],
    },
    ShortcutGroup {
        title: "Actions",
        items: &[
            ("N", "Create a new event"),
            ("/", "Search events"),
            ("?", "Show this keyboard shortcuts window"),
            ("Ctrl+,", "Open preferences"),
        ],
    },
];

/// Opens the read-only keyboard-shortcuts cheat sheet (DESIGN_SPEC.md §12) as a plain
/// modal `adw::Window` rather than `gtk4::ShortcutsWindow` — the latter is deprecated
/// as of GTK 4.18 and, being a sealed composite widget, has no supported way to add
/// the print button the "printable" requirement below needs. Content is duplicated
/// between the on-screen box (`build_shortcuts_content`) and the print routine
/// (`print_shortcuts`) since GTK printing draws directly on a Cairo context rather
/// than rendering existing widgets.
fn show_shortcuts_window(parent: &impl IsA<gtk4::Window>) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(640)
        .title("Keyboard Shortcuts")
        .build();

    let print_button = gtk4::Button::from_icon_name("document-print-symbolic");
    print_button.set_tooltip_text(Some("Print"));
    {
        let window = window.clone();
        print_button.connect_clicked(move |_| print_shortcuts(&window));
    }

    let header = adw::HeaderBar::new();
    header.pack_end(&print_button);

    install_escape_to_close(&window);

    // No fixed `default_height`: the window sizes itself to the legend's natural
    // height (via `propagate_natural_height`) so all three groups fit without a
    // scrollbar at normal text scale, and so a larger Omarchy/GNOME text-scaling
    // factor (which grows every `em`-based size in `load_static_css`, including
    // this content) grows the window instead of clipping into a scrollbar. The
    // `max_content_height` is only a safety net for extreme scaling factors or
    // short displays, where scrolling is preferable to running off-screen.
    let scroller = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(900)
        .child(&build_shortcuts_content())
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroller));

    window.set_content(Some(&toolbar_view));
    window.present();
}

/// The on-screen widget tree for `show_shortcuts_window` — a heading per
/// `ShortcutGroup`, followed by its `<kbd>`-styled key badge and description rows.
fn build_shortcuts_content() -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    root.set_margin_all(18);

    for group in SHORTCUT_GROUPS {
        let heading = gtk4::Label::new(Some(group.title));
        heading.add_css_class("shortcut-group-title");
        heading.set_halign(gtk4::Align::Start);
        root.append(&heading);

        for (key, description) in group.items {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            row.set_margin_top(4);
            row.set_margin_bottom(4);

            let key_label = gtk4::Label::new(Some(key));
            key_label.add_css_class("shortcut-key");
            key_label.set_width_chars(12);
            key_label.set_xalign(0.0);
            row.append(&key_label);

            let desc_label = gtk4::Label::new(Some(description));
            desc_label.set_hexpand(true);
            desc_label.set_halign(gtk4::Align::Start);
            row.append(&desc_label);

            root.append(&row);
        }
    }

    root
}

/// Sends `SHORTCUT_GROUPS` to the system print dialog as a single page of plain
/// Cairo-drawn text — satisfies "printable" directly (the GTK print dialog covers
/// both a real printer and "Print to File" for a PDF) without needing an HTML/PDF
/// rendering path of its own.
fn print_shortcuts(parent: &impl IsA<gtk4::Window>) {
    let op = gtk4::PrintOperation::new();
    op.set_job_name("Calendarchy Keyboard Shortcuts");
    op.connect_begin_print(|op, _ctx| op.set_n_pages(1));
    op.connect_draw_page(|_op, ctx, _page_nr| {
        let cr = ctx.cairo_context();
        let left_margin = 36.0;
        let mut y = 48.0;

        cr.set_source_rgb(0.0, 0.0, 0.0);
        cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Bold);
        cr.set_font_size(18.0);
        cr.move_to(left_margin, y);
        let _ = cr.show_text("Calendarchy — Keyboard Shortcuts");
        y += 36.0;

        for group in SHORTCUT_GROUPS {
            cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Bold);
            cr.set_font_size(13.0);
            cr.move_to(left_margin, y);
            let _ = cr.show_text(group.title);
            y += 22.0;

            cr.select_font_face("Sans", gtk4::cairo::FontSlant::Normal, gtk4::cairo::FontWeight::Normal);
            cr.set_font_size(11.0);
            for (key, description) in group.items {
                cr.move_to(left_margin + 10.0, y);
                let _ = cr.show_text(key);
                cr.move_to(left_margin + 170.0, y);
                let _ = cr.show_text(description);
                y += 18.0;
            }
            y += 14.0;
        }
    });

    if let Err(err) = op.run(gtk4::PrintOperationAction::PrintDialog, Some(parent)) {
        tracing::warn!(%err, "failed to run print operation for keyboard shortcuts");
    }
}

/// A curated shortlist of common IANA zone names for the Preferences window's
/// "Secondary display time zone" picker (DESIGN_SPEC.md §12). Not exhaustive — Week/
/// Day views (the feature this setting will eventually feed a second time gutter
/// into) aren't built yet, so this only needs to cover the common case, not every
/// zone in the tz database.
/// Curated language choices for the Preferences window's "Language" override
/// (DESIGN_SPEC.md §12) — `(display label, BCP-47-ish tag)`. Index 0 in the combo box
/// is always "Automatic (system)" (`None`, not listed here), matching
/// `DISPLAY_TIMEZONE_CHOICES`'s convention below.
const LANGUAGE_CHOICES: &[(&str, &str)] = &[
    ("English (US)", "en-US"),
    ("English (UK)", "en-GB"),
    ("Spanish", "es"),
    ("French", "fr"),
    ("German", "de"),
    ("Italian", "it"),
    ("Portuguese (Brazil)", "pt-BR"),
    ("Dutch", "nl"),
    ("Japanese", "ja"),
    ("Korean", "ko"),
    ("Chinese (Simplified)", "zh-CN"),
    ("Russian", "ru"),
];

/// Curated country choices for the Preferences window's "Country" override
/// (DESIGN_SPEC.md §12) — `(display label, ISO 3166-1 alpha-2 code)`. Same "index 0 is
/// Automatic" convention as `LANGUAGE_CHOICES`.
const COUNTRY_CHOICES: &[(&str, &str)] = &[
    ("United States", "US"),
    ("United Kingdom", "GB"),
    ("Canada", "CA"),
    ("Australia", "AU"),
    ("Germany", "DE"),
    ("France", "FR"),
    ("Spain", "ES"),
    ("Italy", "IT"),
    ("Netherlands", "NL"),
    ("Brazil", "BR"),
    ("Japan", "JP"),
    ("South Korea", "KR"),
    ("China", "CN"),
    ("India", "IN"),
    ("Mexico", "MX"),
];

/// Rough heuristic for "does this system locale conventionally write mm/dd dates and
/// use a 12-hour clock" — true only for US/Canadian English locales, matching the US
/// being the practical outlier globally on both counts. Reads the same `LC_TIME`/
/// `LANG` variables the old read-only Language & Region row read (`system_timezone_name`
/// below does the analogous TZ-based thing for time zones). Good enough as a default
/// for `resolve_date_format`/`resolve_time_format`; anyone who disagrees can override
/// explicitly via the Date format / Time format rows those gate.
fn system_locale_prefers_month_first() -> bool {
    let locale = std::env::var("LC_TIME").or_else(|_| std::env::var("LANG")).unwrap_or_default().to_lowercase();
    locale.starts_with("en_us") || locale.starts_with("en_ca")
}

/// Resolves DESIGN_SPEC.md §12's Date format override to a concrete choice —
/// `DateFormat::System` falls back to `system_locale_prefers_month_first`'s heuristic
/// rather than ever reaching a renderer, so every formatting call site downstream only
/// has to handle the three concrete orders.
fn resolve_date_format(settings: &AppSettings) -> DateFormat {
    match settings.date_format {
        DateFormat::System if system_locale_prefers_month_first() => DateFormat::MonthDayYear,
        DateFormat::System => DateFormat::DayMonthYear,
        other => other,
    }
}

/// Resolves §12's Time format override the same way `resolve_date_format` does.
fn resolve_time_format(settings: &AppSettings) -> TimeFormat {
    match settings.time_format {
        TimeFormat::System if system_locale_prefers_month_first() => TimeFormat::TwelveHour,
        TimeFormat::System => TimeFormat::TwentyFourHour,
        other => other,
    }
}

/// "Aug 31, 2026" / "31 Aug 2026" / "2026-08-31" — the event editor's date-picker
/// button label (`date_picker`), following the resolved Date format override.
fn format_picker_date(date: NaiveDate, format: DateFormat) -> String {
    match format {
        DateFormat::YearMonthDay => date.format("%Y-%m-%d").to_string(),
        DateFormat::DayMonthYear => date.format("%-d %b %Y").to_string(),
        DateFormat::MonthDayYear | DateFormat::System => date.format("%b %-d, %Y").to_string(),
    }
}

/// "Monday, August 31" / "Monday, 31 August" / "Monday, 2026-08-31" — the event
/// popover's date line (`format_event_when`), following the same override. No year in
/// the written-out forms, matching the behavior this replaced.
fn format_popover_date(date: NaiveDate, format: DateFormat) -> String {
    match format {
        DateFormat::YearMonthDay => date.format("%A, %Y-%m-%d").to_string(),
        DateFormat::DayMonthYear => date.format("%A, %-d %B").to_string(),
        DateFormat::MonthDayYear | DateFormat::System => date.format("%A, %B %-d").to_string(),
    }
}

/// "2:30 PM" / "14:30" — event chip (`event_row`) and event popover
/// (`format_event_when`) time display, following the resolved Time format override.
fn format_clock(time: NaiveTime, format: TimeFormat) -> String {
    match format {
        TimeFormat::TwentyFourHour => time.format("%H:%M").to_string(),
        TimeFormat::TwelveHour | TimeFormat::System => time.format("%-I:%M %p").to_string(),
    }
}

/// Maps `DateFormat` to `DATE_FORMAT_LABELS`' index and back, for the Preferences
/// window's Date format `ComboRow` (`show_preferences_window`) — kept as free
/// functions rather than inherent methods on the core crate's `DateFormat` so the
/// app crate's UI-only concern (which label goes at which index) doesn't leak into
/// `calendarchy_core`.
fn date_format_index(format: DateFormat) -> u32 {
    match format {
        DateFormat::System => 0,
        DateFormat::MonthDayYear => 1,
        DateFormat::DayMonthYear => 2,
        DateFormat::YearMonthDay => 3,
    }
}

fn date_format_from_index(index: u32) -> DateFormat {
    match index {
        1 => DateFormat::MonthDayYear,
        2 => DateFormat::DayMonthYear,
        3 => DateFormat::YearMonthDay,
        _ => DateFormat::System,
    }
}

/// Same mapping as `date_format_index`/`date_format_from_index`, for `TimeFormat` and
/// `TIME_FORMAT_LABELS`.
fn time_format_index(format: TimeFormat) -> u32 {
    match format {
        TimeFormat::System => 0,
        TimeFormat::TwelveHour => 1,
        TimeFormat::TwentyFourHour => 2,
    }
}

fn time_format_from_index(index: u32) -> TimeFormat {
    match index {
        1 => TimeFormat::TwelveHour,
        2 => TimeFormat::TwentyFourHour,
        _ => TimeFormat::System,
    }
}

const DISPLAY_TIMEZONE_CHOICES: &[&str] = &[
    "UTC",
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "America/Sao_Paulo",
    "Europe/London",
    "Europe/Paris",
    "Europe/Berlin",
    "Europe/Moscow",
    "Asia/Kolkata",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Australia/Sydney",
    "Pacific/Auckland",
];

/// The full IANA tz database (~600 zones), sorted, backing the searchable time zone
/// pickers — `DISPLAY_TIMEZONE_CHOICES` above is no longer the picker's model, just
/// the seed order `add_zone_row`'s "next unused zone" logic prefers so a freshly
/// added row still defaults to a familiar city. Computed once and cached since it's
/// invariant for the process lifetime.
fn timezone_choices() -> &'static [String] {
    static CHOICES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        let mut zones: Vec<String> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name().to_string()).collect();
        zones.sort();
        zones
    })
}

/// The system time zone's IANA name, for the Preferences window's read-only "System
/// time zone" row (DESIGN_SPEC.md §12 — the grid and new events always use this one;
/// there's no in-app override for it, only the secondary *display* zone alongside it).
/// `chrono::Local` only exposes a UTC offset, not a zone name, so this reads `TZ`
/// first and falls back to resolving `/etc/localtime`'s `zoneinfo/` symlink target —
/// standard on Linux. Falls back to a bare UTC offset if neither is available (e.g. a
/// minimal container image with no tzdata symlink).
fn system_timezone_name() -> String {
    if let Ok(tz) = std::env::var("TZ") {
        if !tz.is_empty() {
            return tz;
        }
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some(pos) = target.find("zoneinfo/") {
            return target[pos + "zoneinfo/".len()..].to_string();
        }
    }
    Local::now().format("UTC%:z").to_string()
}

/// Toggles the `.compact-density` CSS class (rules in `load_static_css`) on the main
/// window — the Preferences window's "Compact density" view option (DESIGN_SPEC.md
/// §12) is the one setting there with an immediate visual effect, so it's applied
/// directly to live widgets rather than requiring a grid re-render.
fn apply_compact_density(window: &adw::Window, enabled: bool) {
    if enabled {
        window.add_css_class("compact-density");
    } else {
        window.remove_css_class("compact-density");
    }
}

/// A friendly display label for an IANA zone name, used by the sidebar's World Clock
/// module and its Preferences rows — e.g. `"America/Los_Angeles"` -> `"Los Angeles"`,
/// `"Europe/London"` -> `"London"`. Takes the last `/`-separated path segment and
/// swaps underscores for spaces; good enough for the curated `DISPLAY_TIMEZONE_CHOICES`
/// list this always feeds from, though it wouldn't prettify every possible IANA name.
fn world_clock_zone_label(zone: &str) -> String {
    zone.rsplit('/').next().unwrap_or(zone).replace('_', " ")
}

/// Maps a zone's local hour (0-23) to a symbolic icon name suggesting time of day,
/// plus a CSS class that tints that icon to match (see `load_static_css`).
/// Bucket boundaries are approximate (no sunrise/sunset calculation) — just a
/// glanceable cue, not astronomically accurate. All four names are confirmed present
/// in the Adwaita icon theme installed on this machine.
fn world_clock_time_of_day_icon(hour: u32) -> (&'static str, &'static str) {
    match hour {
        5..=8 => ("daytime-sunrise-symbolic", "world-clock-icon-sunrise"), // morning
        9..=16 => ("weather-clear-symbolic", "world-clock-icon-day"),     // noon / daytime
        17..=19 => ("daytime-sunset-symbolic", "world-clock-icon-sunset"), // evening
        _ => ("weather-clear-night-symbolic", "world-clock-icon-night"), // twilight / night
    }
}

/// Rebuilds the sidebar's World Clock module (DESIGN_SPEC.md §10/§12) — one row per
/// zone in `settings.world_clock_zones`, in order, each showing that zone's current
/// local time, directly underneath the mini-month date navigator. Hides the whole
/// module — rather than showing an empty header — when the feature is off or no zones
/// are configured yet. Called from `init`/`refresh` (so any settings change shows up
/// on the next natural re-render) and from `start_world_clock_ticker` once a minute so
/// the displayed times stay live in between.
fn populate_world_clock(container: &gtk4::Box, settings: &AppSettings) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    if !settings.world_clock_enabled || settings.world_clock_zones.is_empty() {
        container.set_visible(false);
        return;
    }
    container.set_visible(true);

    let now = Utc::now();
    for zone in &settings.world_clock_zones {
        let row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).build();

        let name_label = gtk4::Label::new(Some(&world_clock_zone_label(zone)));
        name_label.set_halign(gtk4::Align::Start);
        name_label.set_hexpand(true);
        row.append(&name_label);

        let local_time = zone.parse::<Tz>().map(|tz| now.with_timezone(&tz)).ok();

        let (icon_name, icon_css_class) = local_time
            .map(|dt| world_clock_time_of_day_icon(dt.hour()))
            .unwrap_or(("weather-clear-symbolic", "world-clock-icon-day"));

        let time_text = local_time.map(|dt| dt.format("%-I:%M %p").to_string()).unwrap_or_else(|| "--:--".to_string());
        let time_label = gtk4::Label::new(Some(&time_text));
        time_label.add_css_class("dim-label");
        row.append(&time_label);

        let icon = gtk4::Image::from_icon_name(icon_name);
        icon.add_css_class(icon_css_class);
        icon.set_pixel_size(16);
        row.append(&icon);

        container.append(&row);
    }
}

/// Keeps the sidebar's World Clock module's displayed times current — a plain 60s
/// `glib` timer rather than routing through `AppMsg`/`App::refresh` (which would also
/// redo the month grid, sidebar calendar list, etc. every tick for no reason). Settings
/// changes made in Preferences repaint the module immediately instead, via
/// `SettingsCtx::world_clock_box` (see `rebuild_world_clock_zone_rows`), so the two
/// mechanisms cover different triggers — time passing vs. a user edit — rather than
/// duplicating each other.
fn start_world_clock_ticker(world_clock_box: &gtk4::Box, storage: Storage) {
    let world_clock_box = world_clock_box.clone();
    gtk4::glib::timeout_add_local(std::time::Duration::from_secs(60), move || {
        let settings = load_settings(&storage).unwrap_or_default();
        populate_world_clock(&world_clock_box, &settings);
        gtk4::glib::ControlFlow::Continue
    });
}

/// Lights the insertion-line CSS class for `(row, is_before)` on a World Clock zone
/// row, first clearing whatever row/edge `hover_indicator` previously pointed at (if
/// different) so at most one line is ever showing — see `hover_indicator`'s doc
/// comment in `rebuild_world_clock_zone_rows` for why that guard is needed.
fn set_world_clock_drop_indicator(
    hover_indicator: &Rc<RefCell<Option<(adw::ComboRow, bool)>>>,
    row: &adw::ComboRow,
    is_before: bool,
) {
    let mut current = hover_indicator.borrow_mut();
    if let Some((prev_row, prev_before)) = current.as_ref() {
        if *prev_row == *row && *prev_before == is_before {
            return;
        }
    }
    if let Some((prev_row, prev_before)) = current.take() {
        prev_row.remove_css_class(if prev_before { "world-clock-drop-before" } else { "world-clock-drop-after" });
    }
    row.add_css_class(if is_before { "world-clock-drop-before" } else { "world-clock-drop-after" });
    *current = Some((row.clone(), is_before));
}

/// Clears the insertion-line CSS class, but only if `hover_indicator` still points at
/// `row` — guards against a stale `leave` clearing a line that's since moved to a
/// different row (see `set_world_clock_drop_indicator`).
fn clear_world_clock_drop_indicator(hover_indicator: &Rc<RefCell<Option<(adw::ComboRow, bool)>>>, row: &adw::ComboRow) {
    let mut current = hover_indicator.borrow_mut();
    if current.as_ref().map(|(prev_row, _)| prev_row == row).unwrap_or(false) {
        if let Some((prev_row, prev_before)) = current.take() {
            prev_row.remove_css_class(if prev_before { "world-clock-drop-before" } else { "world-clock-drop-after" });
        }
    }
}

/// Tears down and rebuilds the World Clock group's per-zone `adw::ComboRow`s (and the
/// trailing "Add time zone" `adw::ButtonRow`, always kept last) from
/// `settings.world_clock_zones`, in order — the same "clear and repopulate" convention
/// `populate_sidebar`/`populate_mini_calendar` use, rather than patching individual
/// rows in place, so add/remove/reorder are all just "mutate the `Vec`, rebuild" instead
/// of three separate incremental-UI code paths. `rows` tracks exactly the widgets this
/// function itself added to `group` on the previous call, so they (and only they) get
/// removed before the fresh set is added; `add_zone_row` is a single persistent widget
/// reused across rebuilds (its click handler is wired up once, by the caller) rather
/// than recreated here, since it has no per-zone state to go stale.
fn rebuild_world_clock_zone_rows(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<gtk4::Widget>>>,
    settings: &Rc<RefCell<AppSettings>>,
    storage: &Storage,
    world_clock_box: &gtk4::Box,
    add_zone_row: &adw::ButtonRow,
) {
    for widget in rows.borrow_mut().drain(..) {
        group.remove(&widget);
    }
    if add_zone_row.parent().is_some() {
        group.remove(add_zone_row);
    }

    // Tracks whichever single row/edge is currently showing the insertion line, so
    // crossing from one row's bottom half into the next row's top half explicitly
    // clears the old line before showing the new one — each row's `connect_motion`
    // and `connect_leave` fire independently, and GTK doesn't guarantee the outgoing
    // row's `leave` arrives before the incoming row's `motion`, so without this two
    // lines can be lit at once for a moment as the pointer crosses the boundary.
    let hover_indicator: Rc<RefCell<Option<(adw::ComboRow, bool)>>> = Rc::new(RefCell::new(None));

    let zone_count = settings.borrow().world_clock_zones.len();
    for index in 0..zone_count {
        let current_zone = settings.borrow().world_clock_zones[index].clone();
        let choices: Vec<&str> = timezone_choices().iter().map(String::as_str).collect();
        let search_expr =
            gtk4::PropertyExpression::new(gtk4::StringObject::static_type(), None::<&gtk4::Expression>, "string");
        let row = adw::ComboRow::builder()
            .title(format!("Time zone {}", index + 1))
            .model(&gtk4::StringList::new(&choices))
            .enable_search(true)
            .expression(&search_expr)
            .search_match_mode(gtk4::StringFilterMatchMode::Substring)
            .selected(
                timezone_choices()
                    .iter()
                    .position(|z| *z == current_zone)
                    .map(|i| i as u32)
                    .unwrap_or(0),
            )
            .build();
        row.add_css_class("world-clock-row");
        {
            let settings = settings.clone();
            let storage = storage.clone();
            let world_clock_box = world_clock_box.clone();
            row.connect_selected_notify(move |combo| {
                let zone = timezone_choices()[combo.selected() as usize].clone();
                settings.borrow_mut().world_clock_zones[index] = zone;
                if let Err(err) = save_settings(&storage, &settings.borrow()) {
                    tracing::warn!(%err, "failed to save preferences");
                }
                populate_world_clock(&world_clock_box, &settings.borrow());
            });
        }

        let drag_handle = gtk4::Image::from_icon_name("list-drag-handle-symbolic");
        drag_handle.add_css_class("dim-label");
        drag_handle.set_cursor_from_name(Some("grab"));
        drag_handle.set_tooltip_text(Some("Drag to reorder"));

        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
        drag_source.connect_prepare(move |_, _, _| {
            Some(gtk4::gdk::ContentProvider::for_value(&(index as i32).to_value()))
        });
        {
            // Carries the actual row along under the cursor (rather than a generic
            // drag icon) and dims the source row while it's lifted, so the drag reads
            // as picking the row up rather than an instant teleport on drop.
            let row = row.clone();
            drag_source.connect_drag_begin(move |source, _drag| {
                row.add_css_class("world-clock-row-dragging");
                let paintable = gtk4::WidgetPaintable::new(Some(&row));
                source.set_icon(Some(&paintable), row.width() / 2, row.height() / 2);
            });
        }
        {
            let row = row.clone();
            drag_source.connect_drag_end(move |_, _, _| {
                row.remove_css_class("world-clock-row-dragging");
            });
        }
        {
            let row = row.clone();
            drag_source.connect_drag_cancel(move |_, _, _| {
                row.remove_css_class("world-clock-row-dragging");
                false
            });
        }
        drag_handle.add_controller(drag_source);
        row.add_prefix(&drag_handle);

        let drop_target = gtk4::DropTarget::new(i32::static_type(), gtk4::gdk::DragAction::MOVE);
        {
            // Highlights whichever half of the row the pointer is over, so the user
            // sees exactly where the dragged zone will land before releasing. Routed
            // through `hover_indicator` so only ever one row/edge is lit at a time.
            let row = row.clone();
            let hover_indicator = hover_indicator.clone();
            drop_target.connect_motion(move |_, _, y| {
                let is_before = y < row.height() as f64 / 2.0;
                set_world_clock_drop_indicator(&hover_indicator, &row, is_before);
                gtk4::gdk::DragAction::MOVE
            });
        }
        {
            let row = row.clone();
            let hover_indicator = hover_indicator.clone();
            drop_target.connect_leave(move |_| {
                clear_world_clock_drop_indicator(&hover_indicator, &row);
            });
        }
        {
            let row = row.clone();
            let hover_indicator = hover_indicator.clone();
            let group = group.clone();
            let rows = rows.clone();
            let settings = settings.clone();
            let storage = storage.clone();
            let world_clock_box = world_clock_box.clone();
            let add_zone_row = add_zone_row.clone();
            drop_target.connect_drop(move |_, value, _, y| {
                clear_world_clock_drop_indicator(&hover_indicator, &row);
                let Ok(source_index) = value.get::<i32>() else { return false };
                let source_index = source_index as usize;
                // Land before or after this row depending on which half was hovered,
                // shifted left by one if the dragged zone started earlier in the list
                // (removing it first shifts everything after it back by one).
                let drop_after = y >= row.height() as f64 / 2.0;
                let mut insert_at = if drop_after { index + 1 } else { index };
                if source_index < insert_at {
                    insert_at -= 1;
                }
                if insert_at == source_index {
                    return true;
                }
                let mut s = settings.borrow_mut();
                let zone = s.world_clock_zones.remove(source_index);
                s.world_clock_zones.insert(insert_at, zone);
                drop(s);
                if let Err(err) = save_settings(&storage, &settings.borrow()) {
                    tracing::warn!(%err, "failed to save preferences");
                }
                rebuild_world_clock_zone_rows(&group, &rows, &settings, &storage, &world_clock_box, &add_zone_row);
                populate_world_clock(&world_clock_box, &settings.borrow());
                true
            });
        }
        row.add_controller(drop_target);

        let remove_button = gtk4::Button::from_icon_name("user-trash-symbolic");
        remove_button.add_css_class("flat");
        remove_button.set_valign(gtk4::Align::Center);
        remove_button.set_tooltip_text(Some("Remove"));
        {
            let group = group.clone();
            let rows = rows.clone();
            let settings = settings.clone();
            let storage = storage.clone();
            let world_clock_box = world_clock_box.clone();
            let add_zone_row = add_zone_row.clone();
            remove_button.connect_clicked(move |_| {
                settings.borrow_mut().world_clock_zones.remove(index);
                if let Err(err) = save_settings(&storage, &settings.borrow()) {
                    tracing::warn!(%err, "failed to save preferences");
                }
                rebuild_world_clock_zone_rows(&group, &rows, &settings, &storage, &world_clock_box, &add_zone_row);
                populate_world_clock(&world_clock_box, &settings.borrow());
            });
        }
        row.add_suffix(&remove_button);

        group.add(&row);
        // Rebuilds happen wholesale (see doc comment above) rather than moving the
        // widgets that survive a reorder, so every row is freshly created here —
        // fade each one in from the next frame tick rather than popping straight to
        // full opacity, so a drop reads as a settle rather than a flash.
        row.add_css_class("world-clock-row-enter");
        row.add_tick_callback(|widget, _clock| {
            widget.remove_css_class("world-clock-row-enter");
            gtk4::glib::ControlFlow::Break
        });
        rows.borrow_mut().push(row.upcast::<gtk4::Widget>());
    }

    group.add(add_zone_row);
}

/// One row in the Preferences window's left-hand navigation list (see
/// `show_preferences_window` below), paired with the `adw::PreferencesGroup` (or
/// other widget) it scrolls the content pane to.
type SettingsNavAnchors = Rc<RefCell<Vec<(gtk4::ListBoxRow, gtk4::Widget)>>>;

/// One row registered with `wire_settings_nav_search` for as-you-type filtering:
/// every header and item row in insertion order, paired with its lowercased label
/// (compared against the search text) and whether it's a header (headers are
/// filtered by whether any item between them and the next header is still visible,
/// rather than by their own label — see `wire_settings_nav_search`).
struct SettingsSearchRow {
    row: gtk4::ListBoxRow,
    label_lower: String,
    is_header: bool,
}

type SettingsSearchRows = Rc<RefCell<Vec<SettingsSearchRow>>>;

/// A non-selectable, non-activatable row used as a section label in the nav list
/// (e.g. "General", "Add calendar", "Sync") — visually groups the clickable
/// `add_settings_nav_item` rows underneath it without itself being a scroll target.
fn add_settings_nav_header(sidebar: &gtk4::ListBox, search_rows: &SettingsSearchRows, label: &str) {
    let heading = gtk4::Label::new(Some(label));
    heading.add_css_class("heading");
    heading.set_halign(gtk4::Align::Start);
    heading.set_margin_start(12);
    heading.set_margin_top(12);
    heading.set_margin_bottom(2);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&heading));
    row.set_selectable(false);
    row.set_activatable(false);
    row.set_focusable(false);
    sidebar.append(&row);
    search_rows.borrow_mut().push(SettingsSearchRow { row, label_lower: label.to_lowercase(), is_header: true });
}

/// A clickable, indented nav row bound to `target` (typically the first
/// `adw::PreferencesGroup` of a subsection) — registers the pairing in `anchors` so
/// `wire_settings_scrollspy` can scroll to it on click and select it back on scroll,
/// and in `search_rows` so `wire_settings_nav_search` can filter it as-you-type.
fn add_settings_nav_item(
    sidebar: &gtk4::ListBox,
    anchors: &SettingsNavAnchors,
    search_rows: &SettingsSearchRows,
    label: &str,
    target: &gtk4::Widget,
) {
    let item_label = gtk4::Label::new(Some(label));
    item_label.set_halign(gtk4::Align::Start);
    item_label.set_margin_start(24);
    item_label.set_margin_end(12);
    item_label.set_margin_top(6);
    item_label.set_margin_bottom(6);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&item_label));
    sidebar.append(&row);
    anchors.borrow_mut().push((row.clone(), target.clone()));
    search_rows.borrow_mut().push(SettingsSearchRow { row, label_lower: label.to_lowercase(), is_header: false });
}

/// The filtering rule `wire_settings_nav_search` applies on every keystroke, and that
/// its Escape handling (`show_preferences_window`) re-applies immediately with an
/// empty query when clearing the field — rather than waiting on `search-changed`,
/// which per `GtkSearchEntry` docs "is emitted with a delay," so clearing via that
/// alone would leave the list visibly stale for a beat. An item row is shown when its
/// label contains the (lowercased) query, and a header row is shown when the query is
/// empty or at least one item between it and the next header is still visible — so a
/// section heading never lingers above an empty list. Mirrors `copy_to_calendar_button`'s
/// simpler `set_visible`-per-row filtering rather than `gtk4::ListBox::set_filter_func`,
/// since headers need this two-pass visibility rule instead of a plain per-row predicate.
fn apply_settings_nav_filter(query: &str, rows: &[SettingsSearchRow]) {
    let query = query.to_lowercase();
    for entry_row in rows.iter().filter(|r| !r.is_header) {
        entry_row.row.set_visible(query.is_empty() || entry_row.label_lower.contains(&query));
    }
    for (index, entry_row) in rows.iter().enumerate() {
        if !entry_row.is_header {
            continue;
        }
        let any_item_visible = rows[index + 1..].iter().take_while(|r| !r.is_header).any(|r| r.row.is_visible());
        entry_row.row.set_visible(query.is_empty() || any_item_visible);
    }
}

/// Wires a `SearchEntry` sitting above the nav sidebar to filter its rows as-you-type
/// (`apply_settings_nav_filter`). Escape-to-clear for this field is handled separately,
/// by `install_escape_to_close_dialog`, since it also needs to fall through to closing
/// the whole dialog once the field's already empty.
fn wire_settings_nav_search(search_entry: &gtk4::SearchEntry, search_rows: &SettingsSearchRows) {
    let search_rows = search_rows.clone();
    search_entry.connect_search_changed(move |entry| {
        apply_settings_nav_filter(&entry.text(), &search_rows.borrow());
    });
}

/// Wires the nav list and the content pane together both ways: clicking a row scrolls
/// `content` so that row's registered widget meets the top of `scroller`'s viewport
/// (via `row-activated`, which — unlike `row-selected` — never fires from the
/// programmatic `select_row` the scroll-position half below uses, so the two halves
/// don't fight each other); scrolling `content` re-selects whichever row's widget is
/// the last one to have scrolled past the top of the viewport, mirroring the
/// scrollspy behavior of Google Calendar's own Settings page. Must be called after
/// every section has registered its anchors, since it also selects the first one.
fn wire_settings_scrollspy(
    sidebar: &gtk4::ListBox,
    scroller: &gtk4::ScrolledWindow,
    content: &gtk4::Box,
    anchors: &SettingsNavAnchors,
) {
    {
        let anchors = anchors.clone();
        let content = content.clone();
        let scroller = scroller.clone();
        sidebar.connect_row_activated(move |_, row| {
            let target = anchors.borrow().iter().find(|(r, _)| r == row).map(|(_, w)| w.clone());
            if let Some(target) = target {
                if let Some(bounds) = target.compute_bounds(&content) {
                    scroller.vadjustment().set_value(bounds.y() as f64);
                }
            }
        });
    }
    {
        let anchors = anchors.clone();
        let content = content.clone();
        let sidebar = sidebar.clone();
        scroller.vadjustment().connect_value_changed(move |adj| {
            let y = adj.value();
            let mut active: Option<gtk4::ListBoxRow> = None;
            for (row, target) in anchors.borrow().iter() {
                match target.compute_bounds(&content) {
                    Some(bounds) if (bounds.y() as f64) <= y + 32.0 => active = Some(row.clone()),
                    Some(_) => break,
                    None => {}
                }
            }
            if let Some(row) = active {
                sidebar.select_row(Some(&row));
            }
        });
    }
    if let Some((first_row, _)) = anchors.borrow().first() {
        sidebar.select_row(Some(first_row));
    }
}

/// Builds and shows the single Preferences window DESIGN_SPEC.md §12 calls for —
/// opened from the header bar's gear button or Ctrl+, (`install_preferences_shortcut`).
/// One `AppSettings` value (loaded once here) is shared as `Rc<RefCell<_>>` across
/// every row's change handler, mirroring `show_edit_event_dialog`'s `Rc<Cell<_>>`
/// pattern for the date pickers — each row mutates its own field and immediately
/// persists the whole blob, so there's no separate "Save" step (every row applies on
/// change, matching how GNOME's own Settings app behaves).
///
/// Follows the app's established "coming soon" convention (visible-but-disabled rows
/// with an explanatory tooltip/subtitle, e.g. the header bar's Schedule/Week/Year view
/// entries or the event editor's recurrence/notification controls) for settings that
/// don't have a feature to attach to yet — desktop notifications, "Show declined
/// events"/"Show completed tasks", and per-account poll interval overrides — rather
/// than hiding the knob entirely, so the extensibility is visible before those
/// features land (§6's rationale for the same treatment of the Contacts/Notes/Tasks
/// service toggles). World Clock (below) is real, not "coming soon" — it has a
/// sidebar module to attach to (`populate_world_clock`). "Show weekends" is likewise
/// real now (its row below is enabled, unlike the two "Show ..." rows after it) — it
/// feeds `five_day_window`, though only the 5-day view (not yet Month/Week) consults
/// it.
///
/// Laid out as a persistent two-pane shell (`adw::Dialog`, not the old
/// `adw::PreferencesDialog` view-switcher tabs) — a left nav list built by
/// `add_settings_nav_header`/`add_settings_nav_item` and one continuously-scrolling
/// right pane (`content_box`) — mirroring Google Calendar's own Settings page shape:
/// clicking a nav row scrolls to its section, and scrolling highlights the nav row
/// for whatever section is currently in view (`wire_settings_scrollspy`).
/// `adw::PreferencesGroup` doesn't require an `adw::PreferencesPage` parent — it's
/// just a titled boxed-list widget — so every group below still builds exactly the
/// same as it did under the old page-per-tab layout; only the container each group
/// is appended to (`content_box.append` instead of `some_page.add`) changed.
/// The Notifications group's sound-file row subtitle — the picked file's base name
/// when `AppSettings::notification_sound_path` is set, else a note that the built-in
/// default (`notifications::DEFAULT_SOUND_PATH`) is used.
fn sound_row_subtitle(path: Option<&str>) -> String {
    match path {
        Some(path) => format!(
            "Using {}",
            std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path)
        ),
        None => "Using the default sound".into(),
    }
}

/// Guards the Settings dialog's close attempts — the header bar's close button, a
/// window-manager close, and Escape (via `sidebar_search`'s `stop-search` handler
/// below, once its field is already empty) all funnel through `adw::Dialog::close`,
/// which only actually closes while `can-close` stays true. Unlike
/// `install_close_guard`'s `adw::Window::close-request` (a signal that can veto the
/// close directly), `adw::Dialog` guards closing via that boolean property instead:
/// closing succeeds immediately unless something has already flipped `can-close` to
/// false, in which case `close-attempt` fires here instead of the dialog closing, and
/// this is the only place that needs to know what "dirty" means for this window —
/// every field that can mark it dirty (the Add Calendar forms' entry rows, none of
/// which have a working submit button yet to save through) just calls
/// `window.set_can_close(false)` directly rather than threading a separate flag.
fn install_settings_close_guard(window: &adw::Dialog) {
    window.connect_close_attempt(move |window| {
        let confirm = adw::AlertDialog::new(
            Some("Discard unsaved changes?"),
            Some("You've started adding a calendar but haven't finished — closing now discards it."),
        );
        confirm.add_response("keep-editing", "Keep Editing");
        confirm.add_response("discard", "Discard");
        confirm.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("keep-editing"));
        confirm.set_close_response("keep-editing");

        let window_for_response = window.clone();
        confirm.choose(Some(window), gtk4::gio::Cancellable::NONE, move |response| {
            if response == "discard" {
                // `force_close`, not `close` — bypasses the `can-close` check so
                // confirming the discard doesn't just re-trigger this same guard.
                window_for_response.force_close();
            }
        });
    });
}

/// Binds Escape for a dialog that owns a search field, so it always does *something*
/// useful regardless of what currently has focus — unlike the event editor's plain
/// `install_escape_to_close`, this one is two-stage: a first press clears `search_entry`
/// (re-applying its filter immediately, same as `wire_settings_nav_search` does on
/// every keystroke) if it has text, and only once it's already empty does a press
/// close the dialog via `Dialog::close`, still routed through whatever `close-attempt`
/// guard (e.g. `install_settings_close_guard`) is installed on `window`. `Global` scope
/// (mirroring `install_escape_to_close`) is what makes the close-when-empty branch
/// reachable at all when focus is on some other control entirely, not the search field
/// — this is the single source of Escape behavior for the dialog, deliberately not
/// paired with the search entry's own `stop-search` signal, so there's exactly one
/// place deciding what Escape does instead of two handlers racing on the same key.
fn install_escape_to_close_dialog(window: &adw::Dialog, search_entry: &gtk4::SearchEntry, search_rows: &SettingsSearchRows) {
    let controller = gtk4::ShortcutController::new();
    controller.set_scope(gtk4::ShortcutScope::Global);

    let window_for_close = window.clone();
    let search_entry = search_entry.clone();
    let search_rows = search_rows.clone();
    controller.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("Escape"),
        Some(gtk4::CallbackAction::new(move |_widget, _args| {
            if search_entry.text().is_empty() {
                window_for_close.close();
            } else {
                search_entry.set_text("");
                apply_settings_nav_filter("", &search_rows.borrow());
            }
            gtk4::glib::Propagation::Stop
        })),
    ));

    window.add_controller(controller);
}

fn show_preferences_window(ctx: SettingsCtx) {
    let settings = Rc::new(RefCell::new(load_settings(&ctx.storage).unwrap_or_default()));

    let window = adw::Dialog::builder().content_width(820).content_height(680).build();
    install_settings_close_guard(&window);

    let sidebar_search = gtk4::SearchEntry::builder().placeholder_text("Search settings").build();
    sidebar_search.set_margin_start(8);
    sidebar_search.set_margin_end(8);
    sidebar_search.set_margin_top(8);
    sidebar_search.set_margin_bottom(4);

    let sidebar_list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::Single).build();
    sidebar_list.add_css_class("navigation-sidebar");
    let sidebar_scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_width(220)
        .vexpand(true)
        .child(&sidebar_list)
        .build();

    let sidebar_pane = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_pane.append(&sidebar_search);
    sidebar_pane.append(&sidebar_scroller);

    let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 24);
    content_box.set_margin_top(24);
    content_box.set_margin_bottom(24);
    content_box.set_margin_start(24);
    content_box.set_margin_end(24);
    let content_scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .hexpand(true)
        .vexpand(true)
        .child(&content_box)
        .build();

    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.append(&sidebar_pane);
    body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    body.append(&content_scroller);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Settings", "")).build());
    toolbar_view.set_content(Some(&body));
    window.set_child(Some(&toolbar_view));

    let nav_anchors: SettingsNavAnchors = Rc::new(RefCell::new(Vec::new()));
    let nav_search_rows: SettingsSearchRows = Rc::new(RefCell::new(Vec::new()));

    // --- General: language/region overrides (Language/Country stored for future
    // work; Date format/Time format apply immediately, §12), time zone, plus the
    // World clock and keyboard-shortcuts entry points. ---
    add_settings_nav_header(&sidebar_list, &nav_search_rows, "General");

    let region_group = adw::PreferencesGroup::builder()
        .title("Language & Region")
        .description("Overrides the system locale used elsewhere in this app")
        .build();

    let language_labels: Vec<&str> =
        std::iter::once("Automatic (system)").chain(LANGUAGE_CHOICES.iter().map(|(label, _)| *label)).collect();
    let language_combo = adw::ComboRow::builder()
        .title("Language")
        .subtitle("Calendarchy has no translated UI text yet — no visible effect until it does")
        .model(&gtk4::StringList::new(&language_labels))
        .selected(
            settings
                .borrow()
                .language_override
                .as_deref()
                .and_then(|tag| LANGUAGE_CHOICES.iter().position(|(_, t)| *t == tag))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        language_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let tag = (selected > 0).then(|| LANGUAGE_CHOICES[selected as usize - 1].1.to_string());
            settings.borrow_mut().language_override = tag;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    region_group.add(&language_combo);

    let country_labels: Vec<&str> =
        std::iter::once("Automatic (system)").chain(COUNTRY_CHOICES.iter().map(|(label, _)| *label)).collect();
    let country_combo = adw::ComboRow::builder()
        .title("Country")
        .subtitle("Feeds future region-specific features (e.g. holiday calendars, §11) — no effect yet")
        .model(&gtk4::StringList::new(&country_labels))
        .selected(
            settings
                .borrow()
                .country_override
                .as_deref()
                .and_then(|code| COUNTRY_CHOICES.iter().position(|(_, c)| *c == code))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        country_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let code = (selected > 0).then(|| COUNTRY_CHOICES[selected as usize - 1].1.to_string());
            settings.borrow_mut().country_override = code;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    region_group.add(&country_combo);

    // Date format / Time format are the two rows in this group with an immediate
    // visible effect (mirroring `density_row` below being the one immediate-effect
    // row in the View page) — both re-render the main window via `EventUpdated` so a
    // change shows up in the month grid/event popover without reopening Preferences.
    const DATE_FORMAT_LABELS: [&str; 4] = [
        "Automatic (system)",
        "Month/Day/Year (8/31/2026)",
        "Day/Month/Year (31/8/2026)",
        "Year-Month-Day (2026-08-31)",
    ];
    let date_format_combo = adw::ComboRow::builder()
        .title("Date format")
        .model(&gtk4::StringList::new(&DATE_FORMAT_LABELS))
        .selected(date_format_index(settings.borrow().date_format))
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        date_format_combo.connect_selected_notify(move |row| {
            settings.borrow_mut().date_format = date_format_from_index(row.selected());
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    region_group.add(&date_format_combo);

    const TIME_FORMAT_LABELS: [&str; 3] = ["Automatic (system)", "12-hour (1:00 PM)", "24-hour (13:00)"];
    let time_format_combo = adw::ComboRow::builder()
        .title("Time format")
        .model(&gtk4::StringList::new(&TIME_FORMAT_LABELS))
        .selected(time_format_index(settings.borrow().time_format))
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        time_format_combo.connect_selected_notify(move |row| {
            settings.borrow_mut().time_format = time_format_from_index(row.selected());
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    region_group.add(&time_format_combo);

    content_box.append(&region_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Language and region", region_group.upcast_ref());

    // Mirrors Google Calendar's own Settings ▸ Time zone panel: a "Display secondary
    // time zone" checkbox, a primary zone + label row, a secondary zone + label row
    // (sensitive only while the checkbox is on), a swap button between the two, and
    // the "Ask to update my primary time zone to current location" checkbox.
    let timezone_group = adw::PreferencesGroup::builder().title("Time Zone").build();

    let display_secondary_row = adw::SwitchRow::builder()
        .title("Display secondary time zone")
        .active(settings.borrow().display_secondary_timezone)
        .build();

    let system_tz = system_timezone_name();
    let system_choice_label = format!("System default ({system_tz})");
    let primary_choice_labels: Vec<&str> =
        std::iter::once(system_choice_label.as_str()).chain(timezone_choices().iter().map(String::as_str)).collect();
    let primary_search_expr =
        gtk4::PropertyExpression::new(gtk4::StringObject::static_type(), None::<&gtk4::Expression>, "string");
    let primary_combo = adw::ComboRow::builder()
        .title("Primary time zone")
        .subtitle("Used for the calendar grid and new events")
        .model(&gtk4::StringList::new(&primary_choice_labels))
        .enable_search(true)
        .expression(&primary_search_expr)
        .search_match_mode(gtk4::StringFilterMatchMode::Substring)
        .selected(
            settings
                .borrow()
                .primary_timezone
                .as_deref()
                .and_then(|tz| timezone_choices().iter().position(|z| z == tz))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    let primary_label_entry = gtk4::Entry::builder()
        .placeholder_text("Label")
        .width_chars(6)
        .valign(gtk4::Align::Center)
        .text(settings.borrow().primary_timezone_label.as_deref().unwrap_or(""))
        .build();

    let secondary_choice_labels: Vec<&str> =
        std::iter::once("Select a time zone").chain(timezone_choices().iter().map(String::as_str)).collect();
    let secondary_search_expr =
        gtk4::PropertyExpression::new(gtk4::StringObject::static_type(), None::<&gtk4::Expression>, "string");
    let secondary_combo = adw::ComboRow::builder()
        .title("Secondary time zone")
        .subtitle("Adds a second time gutter to Week/Day views — coming soon")
        .model(&gtk4::StringList::new(&secondary_choice_labels))
        .enable_search(true)
        .expression(&secondary_search_expr)
        .search_match_mode(gtk4::StringFilterMatchMode::Substring)
        .sensitive(settings.borrow().display_secondary_timezone)
        .selected(
            settings
                .borrow()
                .secondary_timezone
                .as_deref()
                .and_then(|tz| timezone_choices().iter().position(|z| z == tz))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    let secondary_label_entry = gtk4::Entry::builder()
        .placeholder_text("Label")
        .width_chars(6)
        .valign(gtk4::Align::Center)
        .sensitive(settings.borrow().display_secondary_timezone)
        .text(settings.borrow().secondary_timezone_label.as_deref().unwrap_or(""))
        .build();

    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let secondary_combo = secondary_combo.clone();
        let secondary_label_entry = secondary_label_entry.clone();
        display_secondary_row.connect_active_notify(move |row| {
            let active = row.is_active();
            secondary_combo.set_sensitive(active);
            secondary_label_entry.set_sensitive(active);
            settings.borrow_mut().display_secondary_timezone = active;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    timezone_group.add(&display_secondary_row);

    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        primary_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let tz = (selected > 0).then(|| timezone_choices()[selected as usize - 1].clone());
            settings.borrow_mut().primary_timezone = tz;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        primary_label_entry.connect_changed(move |entry| {
            let text = entry.text();
            settings.borrow_mut().primary_timezone_label = (!text.is_empty()).then(|| text.to_string());
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    primary_combo.add_suffix(&primary_label_entry);

    // Swap button between the primary and secondary rows (the ↕ icon in Google
    // Calendar's own panel) — swaps both the chosen zone and its label, then refreshes
    // both rows' widgets to reflect the swapped state.
    let swap_button = gtk4::Button::from_icon_name("object-flip-vertical-symbolic");
    swap_button.add_css_class("flat");
    swap_button.set_valign(gtk4::Align::Center);
    swap_button.set_tooltip_text(Some("Swap primary and secondary time zones"));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let primary_combo = primary_combo.clone();
        let primary_label_entry = primary_label_entry.clone();
        let secondary_combo = secondary_combo.clone();
        let secondary_label_entry = secondary_label_entry.clone();
        swap_button.connect_clicked(move |_| {
            {
                let mut settings_mut = settings.borrow_mut();
                let settings_mut: &mut AppSettings = &mut settings_mut;
                std::mem::swap(&mut settings_mut.primary_timezone, &mut settings_mut.secondary_timezone);
                std::mem::swap(&mut settings_mut.primary_timezone_label, &mut settings_mut.secondary_timezone_label);
            }
            let settings = settings.borrow();
            primary_combo.set_selected(
                settings
                    .primary_timezone
                    .as_deref()
                    .and_then(|tz| timezone_choices().iter().position(|z| z == tz))
                    .map(|i| (i + 1) as u32)
                    .unwrap_or(0),
            );
            primary_label_entry.set_text(settings.primary_timezone_label.as_deref().unwrap_or(""));
            secondary_combo.set_selected(
                settings
                    .secondary_timezone
                    .as_deref()
                    .and_then(|tz| timezone_choices().iter().position(|z| z == tz))
                    .map(|i| (i + 1) as u32)
                    .unwrap_or(0),
            );
            secondary_label_entry.set_text(settings.secondary_timezone_label.as_deref().unwrap_or(""));
            if let Err(err) = save_settings(&storage, &settings) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    primary_combo.add_suffix(&swap_button);
    timezone_group.add(&primary_combo);

    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        secondary_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let tz = (selected > 0).then(|| timezone_choices()[selected as usize - 1].clone());
            settings.borrow_mut().secondary_timezone = tz;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        secondary_label_entry.connect_changed(move |entry| {
            let text = entry.text();
            settings.borrow_mut().secondary_timezone_label = (!text.is_empty()).then(|| text.to_string());
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    secondary_combo.add_suffix(&secondary_label_entry);
    timezone_group.add(&secondary_combo);

    let ask_update_row = adw::SwitchRow::builder()
        .title("Ask to update my primary time zone to current location")
        .subtitle("Coming soon — no location detection yet")
        .active(settings.borrow().ask_update_primary_timezone_to_location)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        ask_update_row.connect_active_notify(move |row| {
            settings.borrow_mut().ask_update_primary_timezone_to_location = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    timezone_group.add(&ask_update_row);

    content_box.append(&timezone_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Time zone", timezone_group.upcast_ref());

    let world_clock_group = adw::PreferencesGroup::builder()
        .title("World Clock")
        .description("Shown in the sidebar, directly under the mini calendar")
        .build();

    let world_clock_enabled_row =
        adw::SwitchRow::builder().title("Show world clock in sidebar").active(settings.borrow().world_clock_enabled).build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let world_clock_box = ctx.world_clock_box.clone();
        world_clock_enabled_row.connect_active_notify(move |row| {
            settings.borrow_mut().world_clock_enabled = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            populate_world_clock(&world_clock_box, &settings.borrow());
        });
    }
    world_clock_group.add(&world_clock_enabled_row);

    // The zone rows and the trailing "Add time zone" row are all rebuilt together by
    // `rebuild_world_clock_zone_rows` on every add/remove/reorder (see its doc comment)
    // — `world_clock_zone_rows` tracks which widgets that function itself added, and
    // `add_zone_row` is the one persistent widget reused (not recreated) across rebuilds.
    let world_clock_zone_rows: Rc<RefCell<Vec<gtk4::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    let add_zone_row = adw::ButtonRow::builder().title("Add time zone").start_icon_name("list-add-symbolic").build();
    {
        let group = world_clock_group.clone();
        let rows = world_clock_zone_rows.clone();
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let world_clock_box = ctx.world_clock_box.clone();
        let add_zone_row_handle = add_zone_row.clone();
        add_zone_row.connect_activated(move |_| {
            let existing = settings.borrow().world_clock_zones.clone();
            let next_zone = DISPLAY_TIMEZONE_CHOICES
                .iter()
                .find(|zone| !existing.iter().any(|z| z == *zone))
                .unwrap_or(&DISPLAY_TIMEZONE_CHOICES[0])
                .to_string();
            settings.borrow_mut().world_clock_zones.push(next_zone);
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            rebuild_world_clock_zone_rows(&group, &rows, &settings, &storage, &world_clock_box, &add_zone_row_handle);
            populate_world_clock(&world_clock_box, &settings.borrow());
        });
    }
    rebuild_world_clock_zone_rows(
        &world_clock_group,
        &world_clock_zone_rows,
        &settings,
        &ctx.storage,
        &ctx.world_clock_box,
        &add_zone_row,
    );

    content_box.append(&world_clock_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "World clock", world_clock_group.upcast_ref());

    let help_group = adw::PreferencesGroup::builder().title("Keyboard shortcuts").build();
    let shortcuts_row = adw::ActionRow::builder()
        .title("Keyboard shortcuts")
        .subtitle("View the full list of keyboard shortcuts")
        .activatable(true)
        .build();
    shortcuts_row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    {
        let parent = ctx.window.clone();
        shortcuts_row.connect_activated(move |_| show_shortcuts_window(&parent));
    }
    help_group.add(&shortcuts_row);
    content_box.append(&help_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Keyboard shortcuts", help_group.upcast_ref());

    // --- Event settings: new-event defaults (real, and already consumed by the header
    // bar's Create button via `default_new_event`) plus guest/invitation defaults,
    // which stay "coming soon" until the parts of event creation/editing they gate
    // send those fields to the Calendar API. ---
    let defaults_group = adw::PreferencesGroup::builder()
        .title("New Event Defaults")
        .description("Used by the header bar's Create button")
        .build();

    let duration_row = adw::SpinRow::builder()
        .title("Default duration")
        .subtitle("Minutes")
        .build();
    duration_row.set_adjustment(Some(&gtk4::Adjustment::new(
        settings.borrow().default_event_duration_minutes as f64,
        5.0,
        480.0,
        5.0,
        15.0,
        0.0,
    )));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        duration_row.connect_value_notify(move |row| {
            settings.borrow_mut().default_event_duration_minutes = row.value() as i64;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    defaults_group.add(&duration_row);

    let reminder_row = adw::SpinRow::builder()
        .title("Default reminder lead time")
        .subtitle("Minutes before the event — coming soon, until reminders (§13) are implemented")
        .build();
    reminder_row.set_adjustment(Some(&gtk4::Adjustment::new(
        settings.borrow().default_reminder_minutes as f64,
        0.0,
        10080.0,
        5.0,
        15.0,
        0.0,
    )));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        reminder_row.connect_value_notify(move |row| {
            settings.borrow_mut().default_reminder_minutes = row.value() as i64;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    defaults_group.add(&reminder_row);

    let calendars = calendars_by_account(&ctx.storage).unwrap_or_default();
    let mut calendar_labels: Vec<&str> = vec!["Ask each time"];
    calendar_labels.extend(calendars.iter().map(|c| c.display_name.as_str()));
    let calendar_combo = adw::ComboRow::builder()
        .title("Default calendar")
        .model(&gtk4::StringList::new(&calendar_labels))
        .selected(
            settings
                .borrow()
                .default_calendar_id
                .and_then(|id| calendars.iter().position(|c| c.id == id))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let calendars = calendars.clone();
        calendar_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let calendar_id = (selected > 0).then(|| calendars[selected as usize - 1].id);
            settings.borrow_mut().default_calendar_id = calendar_id;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    defaults_group.add(&calendar_combo);

    let auto_add_meet_row = adw::SwitchRow::builder()
        .title("Automatically add Google Meet video conferences to events I create")
        .subtitle("Coming soon — event creation doesn't send conferenceData yet")
        .active(settings.borrow().auto_add_google_meet)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        auto_add_meet_row.connect_active_notify(move |row| {
            settings.borrow_mut().auto_add_google_meet = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    defaults_group.add(&auto_add_meet_row);
    content_box.append(&defaults_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Event settings", defaults_group.upcast_ref());

    // --- Guest permissions: defaults applied to events this account creates, mirroring
    // Google Calendar's "Guest permissions" subsection of Event settings (§12). Stored
    // ahead of the feature — `EventEdits`/`create_event` don't send guest-permission
    // fields to the Calendar API yet, same reasoning as `default_reminder_minutes`. ---
    let guest_group = adw::PreferencesGroup::builder()
        .title("Guest Permissions")
        .description("Applied to events this account creates — coming soon, until event creation sends guest fields to the API")
        .build();

    let guests_modify_row = adw::SwitchRow::builder()
        .title("Modify event")
        .active(settings.borrow().guests_can_modify)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        guests_modify_row.connect_active_notify(move |row| {
            settings.borrow_mut().guests_can_modify = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    guest_group.add(&guests_modify_row);

    let guests_invite_row = adw::SwitchRow::builder()
        .title("Invite others")
        .active(settings.borrow().guests_can_invite_others)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        guests_invite_row.connect_active_notify(move |row| {
            settings.borrow_mut().guests_can_invite_others = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    guest_group.add(&guests_invite_row);

    let guests_see_list_row = adw::SwitchRow::builder()
        .title("See guest list")
        .active(settings.borrow().guests_can_see_guest_list)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        guests_see_list_row.connect_active_notify(move |row| {
            settings.borrow_mut().guests_can_see_guest_list = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    guest_group.add(&guests_see_list_row);
    content_box.append(&guest_group);

    // --- Invitations: how received invitations are handled, mirroring Google
    // Calendar's "Add invitations to my calendar" / "Let others see all invitations"
    // pair in Event settings (§12). Also stored ahead of any feature consuming it. ---
    let invitations_group = adw::PreferencesGroup::builder().title("Invitations").build();

    let invitation_labels = ["All invitations", "Only if the sender is known", "No, only show invitations I've responded to"];
    let invitation_combo = adw::ComboRow::builder()
        .title("Add invitations to my calendar")
        .model(&gtk4::StringList::new(&invitation_labels))
        .selected(match settings.borrow().invitation_auto_add {
            InvitationAutoAdd::All => 0,
            InvitationAutoAdd::KnownSenders => 1,
            InvitationAutoAdd::No => 2,
        })
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        invitation_combo.connect_selected_notify(move |row| {
            settings.borrow_mut().invitation_auto_add = match row.selected() {
                0 => InvitationAutoAdd::All,
                2 => InvitationAutoAdd::No,
                _ => InvitationAutoAdd::KnownSenders,
            };
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    invitations_group.add(&invitation_combo);

    let show_invitations_row = adw::SwitchRow::builder()
        .title("Let others see all invitations if they have permission to view or edit my events")
        .active(settings.borrow().show_all_invitations_to_editors)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        show_invitations_row.connect_active_notify(move |row| {
            settings.borrow_mut().show_all_invitations_to_editors = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    invitations_group.add(&show_invitations_row);
    content_box.append(&invitations_group);

    let notif_group = adw::PreferencesGroup::builder().title("Notifications").build();

    let desktop_notif_row = adw::SwitchRow::builder()
        .title("Desktop notifications")
        .subtitle("Delivered through your desktop's notification system (§13)")
        .active(settings.borrow().desktop_notifications_enabled)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        desktop_notif_row.connect_active_notify(move |row| {
            settings.borrow_mut().desktop_notifications_enabled = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    notif_group.add(&desktop_notif_row);

    let custom_dialog_row = adw::SwitchRow::builder()
        .title("Floating notification popup")
        .subtitle("Calendarchy's own draggable popup, alongside or instead of the desktop toast above")
        .active(settings.borrow().custom_notification_dialog_enabled)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        custom_dialog_row.connect_active_notify(move |row| {
            settings.borrow_mut().custom_notification_dialog_enabled = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    notif_group.add(&custom_dialog_row);

    let play_sound_row = adw::SwitchRow::builder()
        .title("Play a sound")
        .subtitle("Plays the sound below when a reminder fires")
        .active(settings.borrow().play_notification_sounds)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        play_sound_row.connect_active_notify(move |row| {
            settings.borrow_mut().play_notification_sounds = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    notif_group.add(&play_sound_row);

    let sound_row = adw::ActionRow::builder()
        .title("Notification sound")
        .subtitle(sound_row_subtitle(settings.borrow().notification_sound_path.as_deref()))
        .build();
    let choose_sound_button = gtk4::Button::from_icon_name("document-open-symbolic");
    choose_sound_button.add_css_class("flat");
    choose_sound_button.set_valign(gtk4::Align::Center);
    choose_sound_button.set_tooltip_text(Some("Choose a sound file"));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let window = ctx.window.clone();
        let sound_row = sound_row.clone();
        choose_sound_button.connect_clicked(move |_| {
            let dialog = gtk4::FileDialog::builder()
                .title("Choose a notification sound")
                .accept_label("Choose")
                .build();
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("Audio files"));
            filter.add_mime_type("audio/*");
            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter);
            dialog.set_filters(Some(&filters));

            let settings = settings.clone();
            let storage = storage.clone();
            let sound_row = sound_row.clone();
            dialog.open(Some(&window), gtk4::gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                let path = path.to_string_lossy().to_string();
                settings.borrow_mut().notification_sound_path = Some(path.clone());
                if let Err(err) = save_settings(&storage, &settings.borrow()) {
                    tracing::warn!(%err, "failed to save preferences");
                }
                sound_row.set_subtitle(&sound_row_subtitle(Some(path.as_str())));
            });
        });
    }
    sound_row.add_suffix(&choose_sound_button);

    let reset_sound_button = gtk4::Button::from_icon_name("edit-clear-symbolic");
    reset_sound_button.add_css_class("flat");
    reset_sound_button.set_valign(gtk4::Align::Center);
    reset_sound_button.set_tooltip_text(Some("Use the default sound"));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sound_row = sound_row.clone();
        reset_sound_button.connect_clicked(move |_| {
            settings.borrow_mut().notification_sound_path = None;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sound_row.set_subtitle(&sound_row_subtitle(None));
        });
    }
    sound_row.add_suffix(&reset_sound_button);
    notif_group.add(&sound_row);

    let snoozed_row = adw::ActionRow::builder()
        .title("Default snooze duration")
        .subtitle("How long the floating dialog's Snooze button delays a reminder by — its own dropdown offers other durations per-alert")
        .build();

    let (snooze_quantity, snooze_unit_index) = minutes_to_quantity_unit(settings.borrow().default_snooze_minutes, &SNOOZE_UNITS);

    // A `SpinButton`, not `ComboBoxText::with_entry` (deprecated since GTK 4.10) —
    // same "type a small positive integer quantity" widget `add_reminder_row`'s own
    // `quantity_spin` already uses, trading the old preset dropdown for up/down
    // steppers (the adjacent unit dropdown already covers most of the "quick pick"
    // need). Floor of 1, not 0 — unlike a reminder's "0 minutes before" (at event
    // time), a 0-length snooze isn't meaningful.
    let snooze_quantity_spin = gtk4::SpinButton::with_range(1.0, 999.0, 1.0);
    snooze_quantity_spin.set_value(snooze_quantity as f64);
    snooze_quantity_spin.set_width_chars(3);
    snooze_quantity_spin.set_valign(gtk4::Align::Center);

    let snooze_unit_names: Vec<&str> = SNOOZE_UNITS.iter().map(|(name, _)| *name).collect();
    let snooze_unit_dropdown = gtk4::DropDown::from_strings(&snooze_unit_names);
    snooze_unit_dropdown.set_selected(snooze_unit_index);
    snooze_unit_dropdown.set_valign(gtk4::Align::Center);

    let save_snooze_duration = {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let snooze_quantity_spin = snooze_quantity_spin.clone();
        let snooze_unit_dropdown = snooze_unit_dropdown.clone();
        move || {
            let quantity = snooze_quantity_spin.value_as_int() as i64;
            let multiplier = SNOOZE_UNITS[snooze_unit_dropdown.selected() as usize].1;
            settings.borrow_mut().default_snooze_minutes = quantity * multiplier;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        }
    };
    {
        let save_snooze_duration = save_snooze_duration.clone();
        snooze_quantity_spin.connect_value_changed(move |_| save_snooze_duration());
    }
    {
        let save_snooze_duration = save_snooze_duration.clone();
        snooze_unit_dropdown.connect_selected_notify(move |_| save_snooze_duration());
    }

    snoozed_row.add_suffix(&snooze_quantity_spin);
    snoozed_row.add_suffix(&snooze_unit_dropdown);
    notif_group.add(&snoozed_row);

    let rsvp_only_row = adw::SwitchRow::builder()
        .title("Notify me only if I have responded \"Yes\" or \"Maybe\"")
        .subtitle("Coming soon — needs RSVP tracking on synced events, not built yet")
        .active(settings.borrow().notify_only_if_responded)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        rsvp_only_row.connect_active_notify(move |row| {
            settings.borrow_mut().notify_only_if_responded = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    notif_group.add(&rsvp_only_row);

    let reset_position_row = adw::ActionRow::builder()
        .title("Reset popup position")
        .subtitle("Moves the floating popup back to its default corner")
        .build();
    let reset_position_button = gtk4::Button::with_label("Reset");
    reset_position_button.set_valign(gtk4::Align::Center);
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        reset_position_button.connect_clicked(move |_| {
            settings.borrow_mut().notification_dialog_position = None;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    reset_position_row.add_suffix(&reset_position_button);
    notif_group.add(&reset_position_row);

    content_box.append(&notif_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Notification settings", notif_group.upcast_ref());

    // --- View options: density is real and immediate; the rest stay "coming soon"
    // until there's grid/RSVP/Tasks data behind them. ---
    let layout_group = adw::PreferencesGroup::builder().title("Layout").build();
    layout_group.add(
        &adw::ActionRow::builder()
            .title("Default view on launch")
            .subtitle("Month — Day and 5-day views exist now but aren't selectable here yet; Week/Year/Schedule are still on the roadmap")
            .build(),
    );

    let density_row = adw::SwitchRow::builder()
        .title("Compact density")
        .subtitle("Tighter spacing in the calendar grid")
        .active(settings.borrow().compact_density)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let window = ctx.window.clone();
        density_row.connect_active_notify(move |row| {
            let active = row.is_active();
            settings.borrow_mut().compact_density = active;
            apply_compact_density(&window, active);
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    layout_group.add(&density_row);

    let day_time_scale_labels: Vec<String> = DAY_TIME_SCALE_OPTIONS.iter().map(|m| format!("{m} minutes")).collect();
    let day_time_scale_label_refs: Vec<&str> = day_time_scale_labels.iter().map(String::as_str).collect();
    let day_time_scale_combo = adw::ComboRow::builder()
        .title("Time scale")
        .subtitle("Minutes per row in Day view — also adjustable with Ctrl+scroll there")
        .model(&gtk4::StringList::new(&day_time_scale_label_refs))
        .selected(
            DAY_TIME_SCALE_OPTIONS
                .iter()
                .position(|&m| m == settings.borrow().day_time_scale_minutes)
                .unwrap_or(DAY_TIME_SCALE_OPTIONS.len() - 1) as u32,
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        day_time_scale_combo.connect_selected_notify(move |row| {
            settings.borrow_mut().day_time_scale_minutes = DAY_TIME_SCALE_OPTIONS[row.selected() as usize];
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    layout_group.add(&day_time_scale_combo);

    let ctrl_snap_row = adw::SpinRow::builder()
        .title("Ctrl+drag snap interval")
        .subtitle("Minutes — hold Ctrl while dragging an event in Day view")
        .build();
    ctrl_snap_row.set_adjustment(Some(&gtk4::Adjustment::new(
        settings.borrow().day_drag_snap_ctrl_minutes as f64,
        1.0,
        60.0,
        1.0,
        5.0,
        0.0,
    )));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        ctrl_snap_row.connect_value_notify(move |row| {
            settings.borrow_mut().day_drag_snap_ctrl_minutes = row.value() as i64;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    layout_group.add(&ctrl_snap_row);

    let ctrl_shift_snap_row = adw::SpinRow::builder()
        .title("Ctrl+Shift+drag snap interval")
        .subtitle("Minutes — hold Ctrl+Shift while dragging an event in Day view")
        .build();
    ctrl_shift_snap_row.set_adjustment(Some(&gtk4::Adjustment::new(
        settings.borrow().day_drag_snap_ctrl_shift_minutes as f64,
        1.0,
        60.0,
        1.0,
        5.0,
        0.0,
    )));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        ctrl_shift_snap_row.connect_value_notify(move |row| {
            settings.borrow_mut().day_drag_snap_ctrl_shift_minutes = row.value() as i64;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    layout_group.add(&ctrl_shift_snap_row);

    let drag_hold_row = adw::SpinRow::builder()
        .title("Drag hold delay")
        .subtitle("Milliseconds to hold before a click starts moving/resizing an event in Day view")
        .build();
    drag_hold_row.set_adjustment(Some(&gtk4::Adjustment::new(
        settings.borrow().day_drag_hold_ms as f64,
        0.0,
        2000.0,
        50.0,
        100.0,
        0.0,
    )));
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        drag_hold_row.connect_value_notify(move |row| {
            settings.borrow_mut().day_drag_hold_ms = row.value() as i64;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    layout_group.add(&drag_hold_row);

    content_box.append(&layout_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "View options", layout_group.upcast_ref());

    let view_events_group = adw::PreferencesGroup::builder().title("Events").build();
    let show_weekends_row = adw::SwitchRow::builder()
        .title("Show weekends")
        .subtitle("Currently only affects the 5-day view — Month still shows every day")
        .active(settings.borrow().show_weekends)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        show_weekends_row.connect_active_notify(move |row| {
            settings.borrow_mut().show_weekends = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    view_events_group.add(&show_weekends_row);
    let show_declined_row = adw::SwitchRow::builder()
        .title("Show declined events")
        .subtitle("Dims rather than hides — off hides them from the grid entirely")
        .active(settings.borrow().show_declined_events)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        let sender = ctx.sender.clone();
        show_declined_row.connect_active_notify(move |row| {
            settings.borrow_mut().show_declined_events = row.is_active();
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
            sender.input(AppMsg::EventUpdated);
        });
    }
    view_events_group.add(&show_declined_row);
    view_events_group.add(
        &adw::SwitchRow::builder()
            .title("Show completed tasks")
            .subtitle("Coming soon — the Tasks service isn't implemented yet")
            .active(settings.borrow().show_completed_tasks)
            .sensitive(false)
            .build(),
    );
    view_events_group.add(
        &adw::SwitchRow::builder()
            .title("Show week numbers")
            .subtitle("Coming soon")
            .active(settings.borrow().show_week_numbers)
            .sensitive(false)
            .build(),
    );
    view_events_group.add(
        &adw::SwitchRow::builder()
            .title("Display shorter events the same size as 30 minute events")
            .subtitle("Coming soon — the grid doesn't render events yet")
            .active(settings.borrow().uniform_short_event_height)
            .sensitive(false)
            .build(),
    );
    view_events_group.add(
        &adw::SwitchRow::builder()
            .title("Reduce the brightness of past events")
            .subtitle("Coming soon — the grid doesn't render events yet")
            .active(settings.borrow().dim_past_events)
            .sensitive(false)
            .build(),
    );
    view_events_group.add(
        &adw::SwitchRow::builder()
            .title("View calendars side by side in Day View")
            .subtitle("Coming soon — Day view isn't built yet")
            .active(settings.borrow().side_by_side_calendars_in_day_view)
            .sensitive(false)
            .build(),
    );
    content_box.append(&view_events_group);

    // Simple stored preferences (no grid behavior to gate them on yet), so — like the
    // General page's time zone rows above — these stay interactive and persist
    // immediately rather than sitting behind `sensitive(false)`, even though nothing
    // reads them yet.
    let week_group = adw::PreferencesGroup::builder().title("Week & Alternate Calendars").build();

    const START_OF_WEEK_CHOICES: &[(&str, u8)] = &[("Saturday", 6), ("Sunday", 0), ("Monday", 1)];
    let start_of_week_labels: Vec<&str> =
        std::iter::once("Follow region default").chain(START_OF_WEEK_CHOICES.iter().map(|(label, _)| *label)).collect();
    let start_of_week_combo = adw::ComboRow::builder()
        .title("Start week on")
        .model(&gtk4::StringList::new(&start_of_week_labels))
        .selected(
            settings
                .borrow()
                .start_of_week
                .and_then(|day| START_OF_WEEK_CHOICES.iter().position(|(_, d)| *d == day))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        start_of_week_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let day = (selected > 0).then(|| START_OF_WEEK_CHOICES[selected as usize - 1].1);
            settings.borrow_mut().start_of_week = day;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    week_group.add(&start_of_week_combo);

    let custom_view_labels: Vec<String> = (2..=7).map(|n| format!("{n} days")).collect();
    let custom_view_label_refs: Vec<&str> = custom_view_labels.iter().map(String::as_str).collect();
    let custom_view_combo = adw::ComboRow::builder()
        .title("Set custom view")
        .subtitle("Coming soon — the 5-day view (§10) is fixed at 5 days; picking a different day count isn't wired up yet")
        .model(&gtk4::StringList::new(&custom_view_label_refs))
        .selected((settings.borrow().custom_view_days.clamp(2, 7) - 2) as u32)
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        custom_view_combo.connect_selected_notify(move |row| {
            settings.borrow_mut().custom_view_days = row.selected() as i64 + 2;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    week_group.add(&custom_view_combo);

    const ALTERNATE_CALENDAR_CHOICES: &[(&str, &str)] =
        &[("Chinese", "chinese"), ("Hebrew Lunar", "hebrew"), ("Islamic Lunar", "islamic")];
    let alternate_calendar_labels: Vec<&str> =
        std::iter::once("None").chain(ALTERNATE_CALENDAR_CHOICES.iter().map(|(label, _)| *label)).collect();
    let alternate_calendar_combo = adw::ComboRow::builder()
        .title("Alternate calendars")
        .subtitle("Coming soon — no overlay is rendered under grid dates yet")
        .model(&gtk4::StringList::new(&alternate_calendar_labels))
        .selected(
            settings
                .borrow()
                .alternate_calendar
                .as_deref()
                .and_then(|tag| ALTERNATE_CALENDAR_CHOICES.iter().position(|(_, t)| *t == tag))
                .map(|i| (i + 1) as u32)
                .unwrap_or(0),
        )
        .build();
    {
        let settings = settings.clone();
        let storage = ctx.storage.clone();
        alternate_calendar_combo.connect_selected_notify(move |row| {
            let selected = row.selected();
            let tag = (selected > 0).then(|| ALTERNATE_CALENDAR_CHOICES[selected as usize - 1].1.to_string());
            settings.borrow_mut().alternate_calendar = tag;
            if let Err(err) = save_settings(&storage, &settings.borrow()) {
                tracing::warn!(%err, "failed to save preferences");
            }
        });
    }
    week_group.add(&alternate_calendar_combo);

    content_box.append(&week_group);

    // --- Add calendar: Google Calendar's own "+ Add calendar" affordance covers four
    // distinct ways of growing what shows up in the sidebar (DESIGN_SPEC.md §11) —
    // Subscribe to calendar (`calendarList.insert` against an already-connected
    // account), Create new calendar (`calendars.insert`), Browse calendars of interest
    // (a hardcoded shortlist subscribed to the same way as Subscribe, since Google's
    // curated catalog has no public API), and From URL (a read-only ICS feed treated
    // as its own account-less local calendar type). None of the four have a Calendar
    // API call wired up yet, so — matching the "coming soon" convention used
    // throughout this window — every row here is visible but disabled with a tooltip
    // explaining what it's waiting on, rather than hidden. ---
    add_settings_nav_header(&sidebar_list, &nav_search_rows, "Add calendar");

    let subscribe_group = adw::PreferencesGroup::builder()
        .title("Subscribe to calendar")
        .description("Add someone else's calendar by email, once they've shared it with you")
        .build();
    let subscribe_email_row = adw::EntryRow::builder().title("Calendar email or ID").build();
    subscribe_group.add(&subscribe_email_row);
    let subscribe_button_row =
        adw::ButtonRow::builder().title("Subscribe").start_icon_name("list-add-symbolic").sensitive(false).build();
    subscribe_button_row.set_tooltip_text(Some("Coming soon — requires calendarList.insert support (DESIGN_SPEC.md §11)"));
    subscribe_group.add(&subscribe_button_row);
    content_box.append(&subscribe_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Subscribe to calendar", subscribe_group.upcast_ref());

    let create_group = adw::PreferencesGroup::builder().title("Create new calendar").build();
    let create_name_row = adw::EntryRow::builder().title("Name").build();
    create_group.add(&create_name_row);
    let create_description_row = adw::EntryRow::builder().title("Description").build();
    create_group.add(&create_description_row);
    let create_tz_labels: Vec<&str> =
        std::iter::once(system_choice_label.as_str()).chain(timezone_choices().iter().map(String::as_str)).collect();
    let create_tz_search_expr =
        gtk4::PropertyExpression::new(gtk4::StringObject::static_type(), None::<&gtk4::Expression>, "string");
    let create_tz_combo = adw::ComboRow::builder()
        .title("Time zone")
        .model(&gtk4::StringList::new(&create_tz_labels))
        .enable_search(true)
        .expression(&create_tz_search_expr)
        .search_match_mode(gtk4::StringFilterMatchMode::Substring)
        .build();
    create_group.add(&create_tz_combo);
    let create_button_row =
        adw::ButtonRow::builder().title("Create calendar").start_icon_name("list-add-symbolic").sensitive(false).build();
    create_button_row.set_tooltip_text(Some("Coming soon — no calendars.insert call yet (DESIGN_SPEC.md §11)"));
    create_group.add(&create_button_row);
    content_box.append(&create_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Create new calendar", create_group.upcast_ref());

    let browse_group = adw::PreferencesGroup::builder()
        .title("Browse calendars of interest")
        .description("A small hardcoded shortlist (DESIGN_SPEC.md §11) — Google's own curated catalog has no public API")
        .build();
    const BROWSE_CALENDAR_CHOICES: &[&str] = &["Holidays in United States", "Phases of the Moon"];
    for label in BROWSE_CALENDAR_CHOICES {
        browse_group.add(&adw::SwitchRow::builder().title(*label).subtitle("Coming soon").sensitive(false).build());
    }
    content_box.append(&browse_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Browse calendars of interest", browse_group.upcast_ref());

    let from_url_group = adw::PreferencesGroup::builder().title("From URL").build();
    let from_url_row = adw::EntryRow::builder().title("URL of calendar").build();
    from_url_group.add(&from_url_row);
    from_url_group.add(&adw::SwitchRow::builder().title("Make the calendar publicly accessible").sensitive(false).build());
    let add_url_button_row =
        adw::ButtonRow::builder().title("Add calendar").start_icon_name("list-add-symbolic").sensitive(false).build();
    add_url_button_row.set_tooltip_text(Some("Coming soon — no ICS feed fetch/parse support yet (DESIGN_SPEC.md §11)"));
    from_url_group.add(&add_url_button_row);
    content_box.append(&from_url_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "From URL", from_url_group.upcast_ref());

    // None of the four Add Calendar forms above have a working submit button yet
    // (DESIGN_SPEC.md §11) to save whatever's typed into them, so — unlike the rest of
    // this window, which persists every field immediately — text left in any of these
    // is genuine unsaved state. `install_settings_close_guard` reads `can-close`
    // directly, so marking the window dirty is just flipping that property; it flips
    // back once every one of the four is empty again, so clearing what you typed
    // un-guards the close the same way it would with a real "discard" action.
    let unsaved_add_calendar_rows =
        [subscribe_email_row.clone(), create_name_row.clone(), create_description_row.clone(), from_url_row.clone()];
    for row in unsaved_add_calendar_rows.clone() {
        let window = window.clone();
        let unsaved_add_calendar_rows = unsaved_add_calendar_rows.clone();
        row.connect_changed(move |_| {
            let any_text = unsaved_add_calendar_rows.iter().any(|r| !r.text().is_empty());
            window.set_can_close(!any_text);
        });
    }

    // --- Sync: "Sync now" and "Clear local cache and resync" actually drive
    // `Service::sync`/`on_enabled` in the background (DESIGN_SPEC.md §12's Offline
    // section); per-account poll interval overrides stay "coming soon" since there's
    // no scheduled background sync loop yet to apply them to. ---
    add_settings_nav_header(&sidebar_list, &nav_search_rows, "Sync");

    let device_group = adw::PreferencesGroup::builder().title("This Device").build();

    let sync_row = adw::ActionRow::builder()
        .title("Sync now")
        .subtitle("Fetch the latest changes for every connected account")
        .build();
    let sync_button = gtk4::Button::from_icon_name("view-refresh-symbolic");
    sync_button.add_css_class("flat");
    sync_button.set_valign(gtk4::Align::Center);
    sync_button.set_tooltip_text(Some("Sync now"));
    {
        let accounts = ctx.accounts.clone();
        let sender = ctx.sender.clone();
        sync_button.connect_clicked(move |_| {
            sender.oneshot_command(sync_all_accounts(accounts.clone()));
        });
    }
    sync_row.add_suffix(&sync_button);
    device_group.add(&sync_row);

    let clear_row = adw::ActionRow::builder()
        .title("Clear local cache and resync")
        .subtitle("Deletes the local copy of your calendars and events, then re-downloads everything")
        .build();
    let clear_button = gtk4::Button::with_label("Clear & Resync");
    clear_button.add_css_class("destructive-action");
    clear_button.set_valign(gtk4::Align::Center);
    {
        let accounts = ctx.accounts.clone();
        let sender = ctx.sender.clone();
        let window = ctx.window.clone();
        clear_button.connect_clicked(move |_| {
            confirm_clear_cache(&window, accounts.clone(), sender.clone());
        });
    }
    clear_row.add_suffix(&clear_button);
    device_group.add(&clear_row);
    content_box.append(&device_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "This device", device_group.upcast_ref());

    let background_group = adw::PreferencesGroup::builder().title("Background Sync").build();
    background_group.add(
        &adw::ActionRow::builder()
            .title("Poll interval")
            .subtitle("Coming soon — background sync isn't scheduled automatically yet; use Sync Now above")
            .sensitive(false)
            .build(),
    );
    content_box.append(&background_group);
    add_settings_nav_item(&sidebar_list, &nav_anchors, &nav_search_rows, "Background sync", background_group.upcast_ref());

    wire_settings_scrollspy(&sidebar_list, &content_scroller, &content_box, &nav_anchors);
    wire_settings_nav_search(&sidebar_search, &nav_search_rows);
    install_escape_to_close_dialog(&window, &sidebar_search, &nav_search_rows);

    window.present(Some(&ctx.window));
}

/// Pops up a "Clear local cache and resync?" confirmation (same GNOME HIG rationale as
/// `confirm_delete_event`: a destructive-ish action — it throws away the local cache,
/// even though nothing is deleted from Google — shouldn't fire from a single misclick)
/// before spawning the background resync.
fn confirm_clear_cache(window: &adw::Window, accounts: AccountManager, sender: ComponentSender<App>) {
    let dialog = adw::AlertDialog::new(
        Some("Clear local cache and resync?"),
        Some(
            "This deletes your locally cached calendars and events, then re-downloads them from Google. \
             Nothing is deleted from your Google account.",
        ),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("clear", "Clear & Resync");
    dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.choose(Some(window), gtk4::gio::Cancellable::NONE, move |response| {
        if response != "clear" {
            return;
        }
        sender.oneshot_command(clear_cache_and_resync_all(accounts.clone()));
    });
}

/// Runs one `Service::sync` pass (DESIGN_SPEC.md §9) for every account with the
/// Calendar service enabled — the Preferences window's "Sync now" action. Nothing
/// calls `Service::sync` automatically yet (there's no scheduled poll loop, §12's
/// "Background Sync" group), so this is currently the only way a sync ever runs.
async fn sync_all_accounts(accounts: AccountManager) -> AppCommandMsg {
    let mut synced = 0usize;
    let mut failed = 0usize;
    for account in accounts.list_accounts().unwrap_or_default() {
        let enabled = accounts.enabled_services(account.id).unwrap_or_default();
        if !enabled.contains(&ServiceKind::Calendar) {
            continue;
        }
        let Some(service) = accounts.registry().get(ServiceKind::Calendar).cloned() else {
            continue;
        };
        let ctx = accounts.service_context(account.id);
        match service.sync(&ctx).await {
            Ok(()) => synced += 1,
            Err(err) => {
                tracing::warn!(%err, account_id = account.id.0, "sync now: failed to sync account");
                failed += 1;
            }
        }
    }
    AppCommandMsg::SyncFinished { synced, failed }
}

/// Wipes the Calendar service's local cache (`clear_calendar_cache`) and re-populates
/// it via `Service::on_enabled` (re-fetches the calendar list) + `Service::sync`
/// (re-fetches events), for every account with Calendar enabled — the Preferences
/// window's "Clear local cache and resync" recovery action (§12).
async fn clear_cache_and_resync_all(accounts: AccountManager) -> AppCommandMsg {
    let mut cleared = 0usize;
    let mut failed = 0usize;
    for account in accounts.list_accounts().unwrap_or_default() {
        let enabled = accounts.enabled_services(account.id).unwrap_or_default();
        if !enabled.contains(&ServiceKind::Calendar) {
            continue;
        }
        let Some(service) = accounts.registry().get(ServiceKind::Calendar).cloned() else {
            continue;
        };
        let ctx = accounts.service_context(account.id);
        let result: anyhow::Result<()> = async {
            clear_calendar_cache(&ctx.storage, account.id)?;
            service.on_enabled(&ctx).await?;
            service.sync(&ctx).await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => cleared += 1,
            Err(err) => {
                tracing::warn!(%err, account_id = account.id.0, "clear cache and resync failed");
                failed += 1;
            }
        }
    }
    AppCommandMsg::CacheCleared { cleared, failed }
}

/// Removes every child of `widget`. Uses `Widget::unparent`, which works the same way
/// regardless of the container type, rather than each container's own `remove` method
/// — lets `populate_month_grid` (a `Grid`) and `populate_sidebar` (a `Box`) share it.
fn clear_children(widget: &impl IsA<gtk4::Widget>) {
    let widget = widget.as_ref();
    while let Some(child) = widget.first_child() {
        child.unparent();
    }
}

/// The static part of the app's look: hairline month-grid cell borders (rather
/// than a separate card per day), muted weekday/section labels, a today badge in
/// the system accent color (via libadwaita's `@accent_bg_color`/`@accent_fg_color`,
/// so it follows Omarchy's theme automatically — DESIGN_SPEC.md §13), and the
/// sidebar's account-section/calendar-row styling. The sidebar and main calendar
/// area themselves aren't styled here — they use libadwaita's built-in `.card`
/// class directly in the `view!` macro so each floats as its own theme-matched
/// rounded panel over the window background, with that background visible as a
/// gap on every edge. Each pane carries a 12px margin against the *outer* window
/// edges (top/bottom, and its own outer side) but none on the edge it shares with
/// a neighboring pane — there, VS Code-style, the gap between the two cards *is*
/// the resize handle (see the `paned > separator` rule below) rather than a
/// separate thick divider bar layered on top of two more margins. Any future pane
/// should follow the same convention: `.card` + `Overflow::Hidden` + margin on
/// outer edges only, none on edges shared with an adjacent pane's handle. Per-calendar
/// checkbox coloring is layered on top by `load_calendar_color_css`, since it depends on which
/// calendars are actually loaded.
fn load_static_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        "
        /* Sidebar `Paned` handle: like VS Code, there's no separate thick divider
           bar — the handle *is* the gap between the sidebar and main content cards
           (which carry no margin of their own on this shared edge), sized just wide
           enough to show the window background as a seam and stay grabbable. No
           grip dots, no divider line by default; a faint highlight appears only on
           hover/drag so the resize affordance is still discoverable. */
        paned > separator {
            background: none;
            background-image: none;
            border: none;
            box-shadow: none;
            min-width: 8px;
        }
        paned > separator:hover, paned > separator:active {
            background-color: alpha(currentColor, 0.08);
        }
        .notification-overlay-root {
            border: 1px solid alpha(currentColor, 0.15);
            border-radius: 12px;
            background-color: @window_bg_color;
        }
        .notification-overlay {
            border-radius: 12px;
        }
        /* Floating event editor panel (DESIGN_SPEC.md §10's redesigned dockable
           editor): same floating-card look as .notification-overlay-root above
           rather than a new one-off style — it's this app's one other floating
           surface, sitting over the calendar as an editor_overlay overlay child
           instead of a bordered/margined Paned pane, so it needs its own border
           and background where a docked pane gets both for free from .card. */
        .floating-editor-panel {
            border: 1px solid alpha(currentColor, 0.15);
            border-radius: 12px;
            background-color: @window_bg_color;
        }
        /* Live about-to-dock preview (preview_docked_right/preview_docked_bottom) —
           still an overlay child at this point, not yet reparented into the real
           dock slot (see install_panel_drag's doc comment), so without an opaque
           background of its own it would show the calendar content behind it
           bleeding through: a translucent ghost rather than a solid panel. This
           keeps it fully opaque (matching what the panel will look like once really
           docked) but square-cornered/borderless already, so the eventual real dock
           commit at drag-end causes no visible jump. */
        .docking-editor-panel {
            background-color: @window_bg_color;
        }
        /* The event editor panel's own header row is a plain Box, not a real
           HeaderBar (see install_panel_drag's doc comment for why), so it gets none
           of HeaderBar's look for free — this reproduces just the visual part: a
           subtle tint plus a hairline separator from the body below. */
        .event-editor-header {
            background-color: alpha(currentColor, 0.03);
            border-bottom: 1px solid alpha(currentColor, 0.1);
        }
        /* `.flat` alone doesn't fully suppress GtkEntry's border/focus outline in
           this theme, which read as a boxed form field sitting in the header strip —
           this hard-overrides border/background/shadow in every state (including
           focus) so the title truly reads as inline editable text. */
        .event-editor-title-entry,
        .event-editor-title-entry:focus,
        .event-editor-title-entry:focus-within {
            border: none;
            background: none;
            background-color: transparent;
            box-shadow: none;
            outline: none;
        }
        /* The slim titlebar strip above .event-editor-header — a plain, obvious
           grab-anywhere drag handle. Same background as the panel itself
           (@window_bg_color, matching .floating-editor-panel) rather than a
           contrasting tint, so it reads as part of one continuous panel with a
           dedicated drag region at top instead of a mismatched bar; the hairline
           border below is what actually separates it from the header row beneath.
           It carries no label/content of its own (blank by design), so it needs an
           explicit min-height here — without one, a childless Box collapses to
           just its own margins, leaving almost nothing left to see or grab. */
        .event-editor-titlebar {
            background-color: @window_bg_color;
            border-bottom: 1px solid alpha(currentColor, 0.1);
            min-height: 28px;
        }
        menubutton.view-switcher-button {
            border: 1px solid white;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.05);
        }
        .description-frame {
            border: 1px solid alpha(currentColor, 0.15);
            border-radius: 8px;
        }
        .description-frame text {
            padding: 6px 8px;
        }
        .description-toolbar {
            padding-bottom: 2px;
        }
        .description-toolbar button {
            min-width: 28px;
            min-height: 28px;
            padding: 0;
        }
        .search-field {
            border-radius: 999px;
            padding-left: 12px;
            padding-right: 12px;
        }
        .search-field image {
            margin-right: 4px;
        }
        .month-cell {
            border-right: 1px solid alpha(currentColor, 0.12);
            border-bottom: 1px solid alpha(currentColor, 0.12);
            padding: 6px 8px;
        }
        .month-cell.month-cell-drop-target {
            background-color: alpha(@accent_bg_color, 0.12);
        }
        .weekday-label {
            font-size: 0.75em;
            font-weight: 600;
            opacity: 0.55;
            letter-spacing: 0.04em;
        }
        .today-badge {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
            border-radius: 999px;
            min-width: 20px;
            padding: 0 6px;
        }
        .event-row {
            font-size: 0.82em;
            opacity: 0.8;
            border-radius: 4px;
            padding: 1px 3px;
        }
        .event-row:hover {
            background-color: alpha(currentColor, 0.1);
            opacity: 1;
        }
        .event-row.event-row-dragging {
            opacity: 0.35;
        }
        .event-row.event-declined {
            opacity: 0.4;
        }
        .event-badge-row image {
            min-width: 13px;
            min-height: 13px;
        }
        /* Overrides `dim-label`'s 55% opacity for Day view specifically — its cards
           sit on an arbitrary per-calendar accent color (`css_class_for_color`), unlike
           Month view's neutral grid background, so a badge needs guaranteed full-
           strength contrast rather than the muted treatment that reads fine elsewhere.
           More specific than the bare `dim-label` class, so it wins the cascade. */
        .day-event-block .event-badge-row image {
            opacity: 1;
            color: white;
        }
        /* World Clock sidebar row icons (`populate_world_clock`) — tinted per
           time-of-day bucket instead of `dim-label`'s flat gray, so the color
           reinforces the sunrise/day/sunset/night glyph at a glance. */
        image.world-clock-icon-sunrise {
            opacity: 1;
            color: #f6a623;
        }
        image.world-clock-icon-day {
            opacity: 1;
            color: #fdd835;
        }
        image.world-clock-icon-sunset {
            opacity: 1;
            color: #ff7043;
        }
        image.world-clock-icon-night {
            opacity: 1;
            color: white;
        }
        .event-popover contents {
            padding: 0;
        }
        .event-popover-dot {
            min-width: 10px;
            min-height: 10px;
            margin-top: 8px;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.4);
        }
        .event-popover-title {
            font-size: 1.15em;
            font-weight: 600;
        }
        .event-popover-when {
            margin-left: 20px;
        }
        .event-popover-detail-row {
            margin-top: 2px;
        }
        .event-popover-guest-section {
            margin-top: 4px;
        }
        .event-popover-guest-header {
            font-size: 0.78em;
            font-weight: 700;
            opacity: 0.6;
            text-transform: uppercase;
            letter-spacing: 0.04em;
            margin-left: 20px;
        }
        .event-popover-guest-row {
            margin-left: 20px;
        }
        .event-popover-guest-status {
            min-width: 8px;
            min-height: 8px;
            border-radius: 999px;
        }
        .event-popover-guest-status-accepted {
            background-color: #33b679;
        }
        .event-popover-guest-status-declined {
            background-color: #d50000;
        }
        .event-popover-guest-status-tentative {
            background-color: #f6bf26;
        }
        .event-popover-guest-status-needs-action {
            background-color: alpha(currentColor, 0.3);
        }
        .event-dot {
            min-width: 6px;
            min-height: 6px;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.4);
        }
        .more-label {
            font-size: 0.8em;
            font-weight: 600;
            opacity: 0.75;
        }
        .day-summary-header {
            font-size: 0.95em;
            font-weight: 700;
            margin-bottom: 4px;
        }
        .mini-calendar-title {
            font-weight: 600;
        }
        .mini-weekday-label {
            font-size: 0.72em;
            font-weight: 600;
            opacity: 0.55;
        }
        .mini-calendar-day {
            min-width: 26px;
            min-height: 26px;
            padding: 0;
            border-radius: 999px;
        }
        .mini-calendar-today {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
        }
        .mini-calendar-day.mini-calendar-day-in-range {
            background-color: alpha(@accent_bg_color, 0.35);
        }
        .mini-calendar-day.mini-calendar-day-selected {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
        }
        .sidebar-section-header {
            font-size: 0.78em;
            font-weight: 700;
            opacity: 0.6;
            margin-top: 10px;
            text-transform: uppercase;
            letter-spacing: 0.04em;
        }
        .sidebar-list-header > label, .sidebar-section-header-row > label {
            margin-top: 0;
        }
        .sidebar-pill-button {
            min-height: 20px;
            padding: 0 10px;
            font-size: 0.72em;
            font-weight: 600;
        }
        .sidebar-calendar-row {
            padding: 2.4px 4px;
            border-radius: 6px;
        }
        .sidebar-calendar-row:hover {
            background-color: alpha(currentColor, 0.1);
        }
        button.calendar-checkbox {
            min-width: 14.4px;
            min-height: 14.4px;
            padding: 0;
            border-radius: 4px;
            background-color: transparent;
            color: transparent;
            box-shadow: none;
        }
        button.calendar-checkbox:checked {
            color: white;
        }
        .calendar-customizer-button {
            min-width: 14.4px;
            min-height: 14.4px;
            padding: 1.6px;
            border-radius: 9999px;
            opacity: 0.6;
        }
        .calendar-customizer-button:hover, .calendar-customizer-button:checked {
            opacity: 1;
            background-color: alpha(currentColor, 0.1);
        }
        .customizer-menu-row {
            padding: 6px 8px;
            border-radius: 6px;
        }
        .color-swatch {
            min-width: 22px;
            min-height: 22px;
            padding: 0;
        }
        .color-swatch-selected image {
            color: white;
        }
        .event-color-button {
            min-width: 28px;
            min-height: 28px;
            padding: 0;
        }
        .event-color-swatch {
            min-width: 14px;
            min-height: 14px;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.4);
        }
        .event-color-default {
            border: 2px solid alpha(currentColor, 0.35);
            background-color: transparent;
        }
        .reminder-row dropdown {
            min-height: 30px;
        }
        .recur-day-toggle {
            min-width: 32px;
            min-height: 32px;
            padding: 0;
            border-radius: 999px;
        }
        .recur-day-toggle:checked {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
        }
        .shortcut-group-title {
            font-size: 0.85em;
            font-weight: 700;
            opacity: 0.6;
            margin-top: 14px;
            text-transform: uppercase;
            letter-spacing: 0.04em;
        }
        .shortcut-key {
            font-family: monospace;
            font-size: 0.9em;
            padding: 2px 8px;
            border-radius: 6px;
            background-color: alpha(currentColor, 0.08);
            border: 1px solid alpha(currentColor, 0.15);
        }
        .compact-density .month-cell {
            padding: 2px 4px;
        }
        .compact-density .event-row {
            font-size: 0.72em;
            padding: 0px 2px;
        }
        .compact-density .weekday-label {
            font-size: 0.68em;
        }
        .compact-density .sidebar-calendar-row {
            padding: 0.8px 4px;
        }
        .compact-density .sidebar-section-header {
            margin-top: 6px;
        }
        .day-header-row {
            padding: 4px 0 2px 0;
            border: none;
            background-color: transparent;
        }
        .day-header-row * {
            border: none;
            border-left: none;
            border-right: none;
            background-image: none;
            box-shadow: none;
            background-color: transparent;
        }
        .day-tz-label {
            font-size: 0.68em;
            font-weight: 600;
            opacity: 0.5;
            letter-spacing: 0.04em;
            padding-bottom: 4px;
        }
        .day-header-weekday {
            font-size: 0.75em;
            font-weight: 700;
            opacity: 0.6;
            letter-spacing: 0.06em;
        }
        .day-header-weekday-today {
            color: @accent_bg_color;
            opacity: 1;
        }
        .day-header-date {
            font-size: 1.3em;
            font-weight: 500;
        }
        .today-badge-lg {
            background-color: @accent_bg_color;
            color: @accent_fg_color;
            border-radius: 999px;
            min-width: 34px;
            min-height: 34px;
            padding: 2px;
        }
        .day-all-day-strip {
            /* No horizontal padding: the strip's grid must span exactly the same width
               as the hour grid below it, or its day columns (and their dividers) come
               out fractionally narrower and drift left of the hour grid's toward the
               last column. */
            padding: 2px 0 6px 0;
        }
        .day-all-day-event {
            padding: 3px 8px;
        }
        /* Wrapper around an all-day bar's body and its arrow head(s): owns the opacity
           so body and arrow fade/highlight together (see `all_day_event_bar`). */
        .day-all-day-bar {
            opacity: 0.92;
        }
        .day-all-day-bar:hover {
            opacity: 1;
        }
        .day-all-day-bar.event-declined {
            opacity: 0.5;
        }
        /* A clipped edge squares off and loses its border so `all_day_arrow_head`'s
           triangle continues the body's shape seamlessly. Written as a compound
           selector so it outranks `.day-event-block`'s own `border-radius`/`border`
           (declared further down; at equal specificity the later rule would win). */
        .day-event-block.day-all-day-event-clipped-start {
            border-top-left-radius: 0;
            border-bottom-left-radius: 0;
            border-left-width: 0;
            padding-left: 3px;
        }
        .day-event-block.day-all-day-event-clipped-end {
            border-top-right-radius: 0;
            border-bottom-right-radius: 0;
            border-right-width: 0;
            padding-right: 3px;
        }
        .day-all-day-column-divider {
            border-right: 1px solid alpha(currentColor, 0.12);
        }
        .day-hour-label {
            font-size: 0.75em;
            opacity: 0.55;
            padding-right: 8px;
        }
        .day-hour-label-half {
            font-size: 0.70em;
            opacity: 0.48;
        }
        .day-hour-label-quarter {
            font-size: 0.66em;
            opacity: 0.40;
        }
        .day-hour-cell {
            border-top: 1px solid alpha(currentColor, 0.12);
            border-bottom: none;
            border-left: none;
            border-right: none;
            background-image: none;
            box-shadow: none;
        }
        .day-hour-grid * {
            border-left: none;
            border-right: none;
            background-image: none;
            box-shadow: none;
        }
        .day-hour-cell-first {
            border-top: none;
        }
        .day-hour-cell-half {
            border-top: 1px solid alpha(currentColor, 0.09);
        }
        .day-hour-cell-quarter {
            border-top: 1px solid alpha(currentColor, 0.07);
        }
        .day-hour-cell-minor {
            border-top: 1px solid alpha(currentColor, 0.06);
        }
        .day-hour-cell-divider {
            border-right: 1px solid alpha(currentColor, 0.12);
        }
        .day-now-line {
            min-height: 2px;
            background-color: #e8453c;
        }
        .day-now-dot {
            min-width: 8px;
            min-height: 8px;
            border-radius: 999px;
            background-color: #e8453c;
        }
        .day-event-block {
            border-radius: 6px;
            border: 1px solid alpha(black, 0.25);
            color: white;
            opacity: 0.92;
        }
        .day-event-block:hover {
            opacity: 1;
        }
        .day-event-block.event-declined {
            opacity: 0.5;
        }
        /* Inside an all-day bar the wrapper `.day-all-day-bar` already applies the
           opacity above; without this the body would multiply it in a second time. */
        .day-all-day-bar .day-event-block {
            opacity: 1;
        }
        .day-event-subject {
            font-size: 0.85em;
            font-weight: 700;
        }
        .day-event-body {
            font-size: 0.78em;
            opacity: 0.8;
        }
        .day-event-time-bubble {
            font-size: 0.72em;
            font-weight: 600;
            background-color: alpha(black, 0.28);
            border-radius: 999px;
            padding: 1px 8px;
        }
        .day-drag-hint {
            font-size: 0.72em;
            font-weight: 500;
            color: white;
            background-color: alpha(black, 0.75);
            border-radius: 999px;
            padding: 2px 10px;
        }
        .world-clock-row {
            transition: opacity 150ms ease, box-shadow 150ms ease;
        }
        .world-clock-row.world-clock-row-enter {
            opacity: 0;
        }
        .world-clock-row.world-clock-row-dragging {
            opacity: 0.35;
        }
        .world-clock-row.world-clock-drop-before {
            box-shadow: inset 0 2px 0 0 @accent_bg_color;
        }
        .world-clock-row.world-clock-drop-after {
            box-shadow: inset 0 -2px 0 0 @accent_bg_color;
        }
        ",
    );
    add_provider(&provider);
}

/// One CSS rule per distinct calendar color actually in use, so event dots can tag
/// themselves with a `dot-<hex>` class instead of needing per-widget inline styling
/// (which GTK4 doesn't support from Rust the way inline HTML `style=` does). Each
/// color also gets a matching rule for the sidebar's `calendar-checkbox` (§ the
/// single colored-checkbox control replacing the old checkbox+swatch pair): an
/// outline in the calendar's color when unchecked, filled with that color and a
/// white check mark when checked — mirroring the Google Calendar PWA sidebar.
fn load_calendar_color_css(calendars: &[CalendarSummary]) {
    let mut seen = HashSet::new();
    let mut css = String::new();
    for calendar in calendars {
        if let Some(color) = &calendar.color {
            if seen.insert(color.clone()) {
                let class = css_class_for_color(color);
                css.push_str(&format!(".{class} {{ background-color: {color}; }}\n"));
                css.push_str(&format!(
                    "button.calendar-checkbox.{class} {{ box-shadow: inset 0 0 0 2px {color}; }}\n\
                     button.calendar-checkbox.{class}:checked {{ background-color: {color}; box-shadow: none; }}\n"
                ));
            }
        }
    }
    if css.is_empty() {
        return;
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    add_provider(&provider);
}

/// One CSS rule per color in `CALENDAR_COLOR_PALETTE`, registered once at startup so
/// every swatch in `calendar_customizer_button`'s color grid has a background rule
/// even before any calendar has ever used it — unlike `load_calendar_color_css`,
/// which only covers colors calendars are already assigned.
fn load_palette_color_css() {
    let mut css = String::new();
    for (color, _name) in CALENDAR_COLOR_PALETTE {
        let class = css_class_for_color(color);
        css.push_str(&format!(".{class} {{ background-color: {color}; }}\n"));
    }
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    add_provider(&provider);
}

fn css_class_for_color(color: &str) -> String {
    format!("dot-{}", color.trim_start_matches('#'))
}

fn add_provider(provider: &gtk4::CssProvider) {
    let Some(display) = gtk4::gdk::Display::default() else {
        tracing::warn!("no default GDK display; skipping CSS provider registration");
        return;
    };
    gtk4::style_context_add_provider_for_display(&display, provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let core = init_core()?;

    let app = RelmApp::new("org.calendarchy.Calendarchy");
    app.run::<App>(core);

    Ok(())
}

#[cfg(test)]
mod day_view_layout_tests {
    use super::*;

    fn ev(id: i64, start: &str, end: &str) -> DisplayEvent {
        DisplayEvent {
            id,
            title: format!("event {id}"),
            description: None,
            location: None,
            start: start.to_string(),
            end: end.to_string(),
            all_day: false,
            color: None,
            calendar_name: "cal".into(),
            is_recurring: false,
            has_video_call: false,
            is_private: false,
            self_response_status: None,
            reminder_count: 0,
            other_attendee_count: 0,
            other_attendee_names: Vec::new(),
            attachment_count: 0,
        }
    }

    #[test]
    fn isolated_event_gets_full_column() {
        let events = vec![ev(1, "2026-09-01T12:00:00-04:00", "2026-09-01T13:00:00-04:00")];
        let layout = layout_day_events(&events, "2026-09-01");
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].lane, 0);
        assert_eq!(layout[0].columns, 1, "a non-overlapping event should get columns=1 (full width)");
    }

    #[test]
    fn two_overlapping_events_split_in_half() {
        let events = vec![
            ev(1, "2026-09-01T12:00:00-04:00", "2026-09-01T13:00:00-04:00"),
            ev(2, "2026-09-01T12:30:00-04:00", "2026-09-01T13:30:00-04:00"),
        ];
        let layout = layout_day_events(&events, "2026-09-01");
        assert_eq!(layout.len(), 2);
        assert_eq!(layout[0].columns, 2);
        assert_eq!(layout[1].columns, 2);
        assert_ne!(layout[0].lane, layout[1].lane);
    }

    #[test]
    fn three_overlapping_events_split_in_thirds() {
        let events = vec![
            ev(1, "2026-09-01T12:00:00-04:00", "2026-09-01T13:00:00-04:00"),
            ev(2, "2026-09-01T12:10:00-04:00", "2026-09-01T13:00:00-04:00"),
            ev(3, "2026-09-01T12:20:00-04:00", "2026-09-01T13:00:00-04:00"),
        ];
        let layout = layout_day_events(&events, "2026-09-01");
        assert_eq!(layout.len(), 3);
        for item in &layout {
            assert_eq!(item.columns, 3);
        }
        let lanes: HashSet<usize> = layout.iter().map(|i| i.lane).collect();
        assert_eq!(lanes, HashSet::from([0, 1, 2]));
    }

    #[test]
    fn more_than_six_overlaps_cap_at_sixths() {
        // 8 events, all mutually overlapping across a wide window.
        let events: Vec<DisplayEvent> = (0..8)
            .map(|i| ev(i, "2026-09-01T12:00:00-04:00", "2026-09-01T14:00:00-04:00"))
            .collect();
        let layout = layout_day_events(&events, "2026-09-01");
        assert_eq!(layout.len(), 8);
        for item in &layout {
            assert_eq!(item.columns, MAX_DAY_EVENT_COLUMNS);
            assert!(item.lane < MAX_DAY_EVENT_COLUMNS);
        }
    }

    #[test]
    fn non_overlapping_clusters_are_independent() {
        // Two separate double-booked pairs, far apart in the day — each pair should
        // split in half independently, not be affected by the other cluster.
        let events = vec![
            ev(1, "2026-09-01T08:00:00-04:00", "2026-09-01T09:00:00-04:00"),
            ev(2, "2026-09-01T08:30:00-04:00", "2026-09-01T09:30:00-04:00"),
            ev(3, "2026-09-01T18:00:00-04:00", "2026-09-01T19:00:00-04:00"),
            ev(4, "2026-09-01T18:15:00-04:00", "2026-09-01T19:15:00-04:00"),
        ];
        let layout = layout_day_events(&events, "2026-09-01");
        assert_eq!(layout.len(), 4);
        for item in &layout {
            assert_eq!(item.columns, 2, "each independent pair should split in half, not inherit the other cluster's size");
        }
    }

    fn all_day_ev(id: i64, start_date: &str, end_date_exclusive: &str) -> DisplayEvent {
        let mut event = ev(id, start_date, end_date_exclusive);
        event.all_day = true;
        event
    }

    #[test]
    fn day_column_geometries_match_gtk_grid_split() {
        // 1004px overlay, 57px gutter → 947px for 5 hexpand columns: 189 each plus a
        // 2px remainder that GtkGrid gives to the first two columns, one pixel each.
        let columns = day_column_geometries(1004, 57, 5);
        assert_eq!(columns.len(), 5);
        assert_eq!(columns.iter().map(|c| c.width_px).collect::<Vec<_>>(), vec![190, 190, 189, 189, 189]);
        assert_eq!(columns[0].x_offset_px, 57, "first day column starts right after the gutter");
        for pair in columns.windows(2) {
            assert_eq!(pair[1].x_offset_px, pair[0].x_offset_px + pair[0].width_px, "columns must tile with no gaps");
        }
        let last = columns.last().unwrap();
        assert_eq!(last.x_offset_px + last.width_px, 1004, "columns must fill the overlay exactly");
    }

    #[test]
    fn day_column_geometries_single_day_takes_everything_past_the_gutter() {
        assert_eq!(day_column_geometries(800, 52, 1), vec![DayColumnGeometry { width_px: 748, x_offset_px: 52 }]);
    }

    #[test]
    fn day_column_geometries_floors_an_unlaid_out_first_frame() {
        // Width 0 (before the first real layout pass) still yields 60px-per-day columns
        // rather than zero/negative widths.
        let columns = day_column_geometries(0, 52, 5);
        assert!(columns.iter().all(|c| c.width_px == 60));
        assert_eq!(columns[4].x_offset_px, 52 + 4 * 60);
    }

    #[test]
    fn single_day_all_day_event_lines_up_under_its_own_column() {
        let events = vec![all_day_ev(1, "2026-09-02", "2026-09-03")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].row, 0);
        assert_eq!(layout[0].start_col, 1, "Sep 2 is column index 1 in a Sep1..5 window");
        assert_eq!(layout[0].span_cols, 1);
        assert!(!layout[0].clipped_start);
        assert!(!layout[0].clipped_end);
    }

    #[test]
    fn multiday_event_spanning_the_whole_window_gets_full_span() {
        let events = vec![all_day_ev(1, "2026-09-01", "2026-09-06")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].start_col, 0);
        assert_eq!(layout[0].span_cols, 5, "a strip covering every visible date should span all 5 columns");
        assert!(!layout[0].clipped_start);
        assert!(!layout[0].clipped_end);
    }

    #[test]
    fn multiday_event_starting_before_the_window_is_clipped_at_the_left_edge() {
        let events = vec![all_day_ev(1, "2026-08-28", "2026-09-03")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].start_col, 0, "clipped to the window's first column, not its real (off-screen) start");
        assert_eq!(layout[0].span_cols, 2);
        assert!(layout[0].clipped_start);
        assert!(!layout[0].clipped_end);
    }

    #[test]
    fn multiday_event_ending_after_the_window_is_clipped_at_the_right_edge() {
        let events = vec![all_day_ev(1, "2026-09-04", "2026-09-10")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].start_col, 3);
        assert_eq!(layout[0].span_cols, 2, "clipped to the window's last column, not its real (off-screen) end");
        assert!(!layout[0].clipped_start);
        assert!(layout[0].clipped_end);
    }

    #[test]
    fn event_entirely_outside_the_window_is_dropped() {
        let events = vec![all_day_ev(1, "2026-09-10", "2026-09-12")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        assert!(layout_all_day_events(&events, &window).is_empty());
    }

    #[test]
    fn two_overlapping_multiday_events_land_on_different_rows() {
        let events = vec![all_day_ev(1, "2026-09-01", "2026-09-04"), all_day_ev(2, "2026-09-02", "2026-09-05")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 2);
        assert_ne!(layout[0].row, layout[1].row, "overlapping date ranges must not share a row");
    }

    #[test]
    fn non_overlapping_multiday_events_share_a_row() {
        let events = vec![all_day_ev(1, "2026-09-01", "2026-09-03"), all_day_ev(2, "2026-09-03", "2026-09-05")];
        let window = [d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)];
        let layout = layout_all_day_events(&events, &window);
        assert_eq!(layout.len(), 2);
        assert_eq!(layout[0].row, 0);
        assert_eq!(layout[1].row, 0, "back-to-back (non-overlapping) events should pack into the same row");
    }

    #[test]
    fn cycle_day_time_scale_steps_through_options_and_clamps() {
        assert_eq!(cycle_day_time_scale(60, true), 30);
        assert_eq!(cycle_day_time_scale(30, true), 15);
        assert_eq!(cycle_day_time_scale(15, true), 10);
        assert_eq!(cycle_day_time_scale(10, true), 5);
        assert_eq!(cycle_day_time_scale(5, true), 5, "already finest, stays clamped");
        assert_eq!(cycle_day_time_scale(5, false), 10);
        assert_eq!(cycle_day_time_scale(60, false), 60, "already coarsest, stays clamped");
    }

    fn d(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date")
    }

    #[test]
    fn five_day_window_with_weekends_centers_on_anchor() {
        // Thu 2026-09-03, matching the reference screenshot: Tue Sep1 .. Sat Sep5,
        // with the anchor (today) as the 3rd of 5 columns.
        let window = five_day_window(d(2026, 9, 3), true);
        assert_eq!(window, vec![d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)]);
    }

    #[test]
    fn five_day_window_without_weekends_skips_saturday_and_sunday() {
        // Thu 2026-09-03 anchor: 2 business days before/after, no Sat/Sun.
        let window = five_day_window(d(2026, 9, 3), false);
        assert_eq!(window, vec![d(2026, 9, 1), d(2026, 9, 2), d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 7)]);
    }

    #[test]
    fn five_day_window_without_weekends_snaps_a_weekend_anchor_forward() {
        // Sat 2026-09-05 anchor snaps forward to Mon 2026-09-07 before centering.
        let window = five_day_window(d(2026, 9, 5), false);
        assert_eq!(window, vec![d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 7), d(2026, 9, 8), d(2026, 9, 9)]);
    }

    #[test]
    fn step_weekdays_skips_weekends_in_both_directions() {
        assert_eq!(step_weekdays(d(2026, 9, 3), 5), d(2026, 9, 10), "Thu +5 weekdays lands on the next Thu");
        assert_eq!(step_weekdays(d(2026, 9, 3), -5), d(2026, 8, 27), "Thu -5 weekdays lands on the previous Thu");
        assert_eq!(step_weekdays(d(2026, 9, 4), 1), d(2026, 9, 7), "Fri +1 weekday skips the weekend to Mon");
    }

    #[test]
    fn five_day_title_within_one_month() {
        let dates = five_day_window(d(2026, 9, 3), true);
        assert_eq!(five_day_title(&dates), "September 2026");
    }

    #[test]
    fn five_day_title_spans_a_month_boundary() {
        // Mon 2026-08-31 anchor: window spans Aug 29 .. Sep 2.
        let dates = five_day_window(d(2026, 8, 31), true);
        assert_eq!(five_day_title(&dates), "Aug 29 – Sep 2, 2026");
    }

    #[test]
    fn five_day_title_spans_a_year_boundary() {
        // Thu 2026-01-01 anchor: window spans Dec 30, 2025 .. Jan 3, 2026.
        let dates = five_day_window(d(2026, 1, 1), true);
        assert_eq!(five_day_title(&dates), "Dec 30, 2025 – Jan 3, 2026");
    }

    #[test]
    fn capped_date_range_single_date_when_other_equals_origin() {
        assert_eq!(capped_date_range(d(2026, 9, 3), d(2026, 9, 3), 8), vec![d(2026, 9, 3)]);
    }

    #[test]
    fn capped_date_range_within_cap_is_inclusive_both_directions() {
        assert_eq!(
            capped_date_range(d(2026, 9, 3), d(2026, 9, 5), 8),
            vec![d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)]
        );
        assert_eq!(
            capped_date_range(d(2026, 9, 5), d(2026, 9, 3), 8),
            vec![d(2026, 9, 3), d(2026, 9, 4), d(2026, 9, 5)],
            "origin after other still returns the same dates in order"
        );
    }

    #[test]
    fn capped_date_range_exactly_at_cap_boundary() {
        // origin .. origin+7 is exactly 8 dates — the cap, not one over it.
        let range = capped_date_range(d(2026, 9, 1), d(2026, 9, 8), 8);
        assert_eq!(range, (1..=8).map(|day| d(2026, 9, day)).collect::<Vec<_>>());
    }

    #[test]
    fn capped_date_range_trims_the_far_end_beyond_cap() {
        // Dragging 10 days forward from the origin still only keeps 8, anchored on it.
        let range = capped_date_range(d(2026, 9, 1), d(2026, 9, 11), 8);
        assert_eq!(range, (1..=8).map(|day| d(2026, 9, day)).collect::<Vec<_>>());

        // Same, dragging backward — origin is kept as the *last* date instead.
        let range = capped_date_range(d(2026, 9, 20), d(2026, 9, 1), 8);
        assert_eq!(range, (13..=20).map(|day| d(2026, 9, day)).collect::<Vec<_>>());
    }

    #[test]
    fn edge_zone_caps_at_a_third_of_card_height() {
        assert_eq!(day_drag_edge_zone_px(60), 10, "capped at DAY_EVENT_EDGE_GRAB_PX, not a third of 60");
        assert_eq!(day_drag_edge_zone_px(18), 6, "a third of the shortest card height (18px)");
        assert_eq!(day_drag_edge_zone_px(1), 1, "floored at 1px even for a near-zero-height card");
    }

    #[test]
    fn classify_day_drag_zone_resolves_edges_and_body() {
        assert_eq!(classify_day_drag_zone(0.0, 60), DayDragZone::ResizeTop);
        assert_eq!(classify_day_drag_zone(10.0, 60), DayDragZone::ResizeTop, "boundary is inclusive");
        assert_eq!(classify_day_drag_zone(11.0, 60), DayDragZone::Move);
        assert_eq!(classify_day_drag_zone(49.0, 60), DayDragZone::Move);
        assert_eq!(classify_day_drag_zone(50.0, 60), DayDragZone::ResizeBottom);
        assert_eq!(classify_day_drag_zone(60.0, 60), DayDragZone::ResizeBottom);

        // A short (18px) card: edge zones (6px each) must not swallow the middle.
        assert_eq!(classify_day_drag_zone(3.0, 18), DayDragZone::ResizeTop);
        assert_eq!(classify_day_drag_zone(9.0, 18), DayDragZone::Move);
        assert_eq!(classify_day_drag_zone(15.0, 18), DayDragZone::ResizeBottom);
    }

    #[test]
    fn snap_minutes_rounds_to_nearest_scale_increment() {
        assert_eq!(snap_minutes_since_midnight(7.0, 15), 0);
        assert_eq!(snap_minutes_since_midnight(23.0, 15), 30);
        assert_eq!(snap_minutes_since_midnight(37.0, 15), 30);
        assert_eq!(snap_minutes_since_midnight(38.0, 15), 45);
        assert_eq!(snap_minutes_since_midnight(29.0, 60), 0);
        assert_eq!(snap_minutes_since_midnight(30.0, 60), 60, "half-way rounds up");
    }

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime<Local> {
        local_datetime(date, NaiveTime::from_hms_opt(hour, minute, 0).expect("valid time"))
    }

    /// Single-day-view `dates`/`day_column_width`/`day_index` fixture for tests that
    /// only exercise vertical (time-of-day) drag behavior — with only one column,
    /// `compute_day_drag_times` can never resolve a different target day, so these
    /// match the pre-cross-day-drag behavior exactly.
    fn single_day(date: NaiveDate) -> (Vec<NaiveDate>, f64, usize) {
        (vec![date], 100.0, 0)
    }

    #[test]
    fn compute_drag_move_snaps_to_grid_and_preserves_duration() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(date, 12, 0), original_end: dt(date, 13, 0) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, 22.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 12, 15));
        assert_eq!(result.end, dt(date, 13, 15), "duration must stay exactly 1h");
    }

    #[test]
    fn compute_drag_move_unsnapped_tracks_the_pointer_continuously() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(date, 12, 0), original_end: dt(date, 13, 0) };
        let (dates, width, index) = single_day(date);

        // `None` (live-feedback path): rounds to the nearest whole minute, not the
        // nearest `scale_minutes` gridline — a 7-minute drag lands on 12:07, not
        // snapped away to 12:00/12:15 the way `Some(15)` would.
        let result = compute_day_drag_times(&state, 0.0, 7.0, 1.0, width, &dates, index, None);

        assert_eq!(result.start, dt(date, 12, 7));
        assert_eq!(result.end, dt(date, 13, 7));
    }

    #[test]
    fn compute_drag_move_clamps_at_midnight() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(date, 0, 5), original_end: dt(date, 1, 5) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, -1000.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 0, 0), "can't move before the start of the day");
        assert_eq!(result.end, dt(date, 1, 0), "duration preserved even when clamped");
    }

    #[test]
    fn compute_drag_move_clamps_at_day_end() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(date, 22, 0), original_end: dt(date, 23, 0) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, 1000.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 23, 0), "can't move past the end of the day");
        assert_eq!(result.end, dt(date, 0, 0) + Duration::days(1), "clamped end lands exactly at midnight");
    }

    #[test]
    fn compute_drag_resize_top_moves_start_only() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::ResizeTop, original_start: dt(date, 12, 0), original_end: dt(date, 13, 0) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, -22.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 11, 45));
        assert_eq!(result.end, dt(date, 13, 0), "end must not move");
    }

    #[test]
    fn compute_drag_resize_top_clamps_at_min_duration() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        // A short 20-minute event: dragging the start far down can't compress it past 15m.
        let state = DayDragState { zone: DayDragZone::ResizeTop, original_start: dt(date, 12, 0), original_end: dt(date, 12, 20) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, 1000.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 12, 5), "clamped to end minus the 15-minute floor");
        assert_eq!(result.end, dt(date, 12, 20));
    }

    #[test]
    fn compute_drag_resize_bottom_clamps_at_min_duration() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).expect("valid date");
        let state = DayDragState { zone: DayDragZone::ResizeBottom, original_start: dt(date, 12, 0), original_end: dt(date, 12, 20) };
        let (dates, width, index) = single_day(date);

        let result = compute_day_drag_times(&state, 0.0, -1000.0, 1.0, width, &dates, index, Some(15));

        assert_eq!(result.start, dt(date, 12, 0));
        assert_eq!(result.end, dt(date, 12, 15), "clamped to start plus the 15-minute floor");
    }

    #[test]
    fn compute_drag_move_crosses_into_the_next_column() {
        let mon = NaiveDate::from_ymd_opt(2026, 9, 7).expect("valid date"); // a Monday
        let dates: Vec<NaiveDate> = (0..5).map(|i| mon + Duration::days(i)).collect();
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(mon, 12, 0), original_end: dt(mon, 13, 0) };

        // One column width to the right, no vertical movement: lands on Tuesday at
        // the same time, and `column_index` reflects the new column.
        let result = compute_day_drag_times(&state, 100.0, 0.0, 1.0, 100.0, &dates, 0, Some(15));

        assert_eq!(result.start, dt(dates[1], 12, 0));
        assert_eq!(result.end, dt(dates[1], 13, 0));
        assert_eq!(result.column_index, 1);
    }

    #[test]
    fn compute_drag_move_column_crossing_skips_hidden_weekends() {
        // `five_day_window` can produce non-contiguous dates when weekends are
        // hidden (Friday's column is immediately followed by Monday's, a 3-day
        // calendar gap) — the column index must drive the target day, not raw
        // day-arithmetic on `offset_x`.
        let fri = NaiveDate::from_ymd_opt(2026, 9, 4).expect("valid date");
        let mon = NaiveDate::from_ymd_opt(2026, 9, 7).expect("valid date");
        let dates = vec![fri, mon];
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(fri, 12, 0), original_end: dt(fri, 13, 0) };

        let result = compute_day_drag_times(&state, 100.0, 0.0, 1.0, 100.0, &dates, 0, Some(15));

        assert_eq!(result.start, dt(mon, 12, 0), "crossing one column jumps the full Fri->Mon gap");
        assert_eq!(result.column_index, 1);
    }

    #[test]
    fn compute_drag_move_column_crossing_clamps_to_visible_range() {
        let mon = NaiveDate::from_ymd_opt(2026, 9, 7).expect("valid date");
        let dates: Vec<NaiveDate> = (0..5).map(|i| mon + Duration::days(i)).collect();
        let state = DayDragState { zone: DayDragZone::Move, original_start: dt(mon, 12, 0), original_end: dt(mon, 13, 0) };

        // Dragging far past the last visible column clamps to it rather than
        // resolving to a date outside `dates`.
        let result = compute_day_drag_times(&state, 10_000.0, 0.0, 1.0, 100.0, &dates, 0, Some(15));

        assert_eq!(result.column_index, dates.len() - 1);
        assert_eq!(result.start, dt(dates[dates.len() - 1], 12, 0));
    }

    #[test]
    fn compute_drag_resize_ignores_horizontal_movement() {
        let mon = NaiveDate::from_ymd_opt(2026, 9, 7).expect("valid date");
        let dates: Vec<NaiveDate> = (0..5).map(|i| mon + Duration::days(i)).collect();
        let state = DayDragState { zone: DayDragZone::ResizeBottom, original_start: dt(mon, 12, 0), original_end: dt(mon, 13, 0) };

        // A large horizontal offset must not move a resize drag to another day.
        let result = compute_day_drag_times(&state, 500.0, 0.0, 1.0, 100.0, &dates, 0, Some(15));

        assert_eq!(result.column_index, 0);
        assert_eq!(result.start.date_naive(), mon);
        assert_eq!(result.end.date_naive(), mon);
    }

    #[test]
    fn shift_date_string_preserves_time_of_day_for_timed_events() {
        assert_eq!(shift_date_string("2026-09-01T12:30:00+00:00", 3).as_deref(), Some("2026-09-04T12:30:00+00:00"));
    }

    #[test]
    fn shift_date_string_stays_a_plain_date_for_all_day_events() {
        assert_eq!(shift_date_string("2026-09-01", 3).as_deref(), Some("2026-09-04"));
    }

    #[test]
    fn shift_date_string_rejects_unparseable_input() {
        assert_eq!(shift_date_string("not a date", 1), None);
    }
}
