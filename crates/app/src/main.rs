use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use calendarchy_core::{AccountManager, Keyring, SecretServiceKeyring, ServiceRegistry, Storage};
use calendarchy_service_calendar::query::{events_for_visible_calendars, DisplayEvent};
use calendarchy_service_calendar::CalendarService;
use chrono::{Datelike, Duration, Local, NaiveDate};
use gtk4::prelude::*;
use relm4::adw;
use relm4::prelude::*;

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

struct App {
    core: AppCore,
}

#[derive(Debug)]
enum AppMsg {}

#[relm4::component]
impl SimpleComponent for App {
    type Init = AppCore;
    type Input = AppMsg;
    type Output = ();

    view! {
        adw::Window {
            set_title: Some("Calendarchy"),
            set_default_width: 1000,
            set_default_height: 720,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {
                    #[wrap(Some)]
                    set_title_widget = &adw::WindowTitle {
                        set_title: &month_title,
                    },
                },

                #[wrap(Some)]
                set_content = &gtk4::ScrolledWindow {
                    set_vexpand: true,
                    set_hexpand: true,

                    #[name = "month_grid"]
                    gtk4::Grid {
                        add_css_class: "month-grid",
                        set_row_homogeneous: false,
                        set_column_homogeneous: true,
                        set_hexpand: true,
                        set_vexpand: true,
                    },
                },
            }
        }
    }

    fn init(core: Self::Init, root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let accounts = core.core_accounts_summary();
        tracing::info!(count = accounts, "loaded accounts at startup");

        let today = Local::now().date_naive();
        let month_title = today.format("%B %Y").to_string();

        let events = events_for_visible_calendars(&core.storage).unwrap_or_default();
        tracing::info!(count = events.len(), "loaded events for month view");

        load_static_css();
        load_calendar_color_css(&events);

        let model = App { core };
        let widgets = view_output!();

        populate_month_grid(&widgets.month_grid, today, &events);

        let _ = sender;
        ComponentParts { model, widgets }
    }

    fn update(&mut self, _msg: Self::Input, _sender: ComponentSender<Self>) {}
}

impl AppCore {
    fn core_accounts_summary(&self) -> usize {
        self.accounts.list_accounts().map(|a| a.len()).unwrap_or(0)
    }
}

/// Fills a `gtk4::Grid` with a flat, borderless-but-hairlined Sunday-first month
/// calendar (DESIGN_SPEC.md §10's Month view): complete weeks, padded with the
/// tail/head of the adjacent months so every row is a full 7 days, weekday
/// abbreviations folded into the first row's cells rather than a separate header row,
/// and up to four compact `<dot> <time> <title>` rows per day before collapsing into
/// a "N more" line. Events are grouped by `DisplayEvent::start_date()` client-side
/// rather than via a per-day SQL query — see `calendarchy_service_calendar::query`.
fn populate_month_grid(grid: &gtk4::Grid, today: NaiveDate, events: &[DisplayEvent]) {
    const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    const MAX_VISIBLE_EVENTS: usize = 4;

    let mut events_by_day: HashMap<&str, Vec<&DisplayEvent>> = HashMap::new();
    for event in events {
        events_by_day.entry(event.start_date()).or_default().push(event);
    }

    let year = today.year();
    let month = today.month();
    let first_of_month = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month");
    let days_in_month = days_in_month(year, month);
    let leading = first_of_month.weekday().num_days_from_sunday();
    let total_cells = leading + days_in_month;
    let rows = total_cells.div_ceil(7);
    let grid_start = first_of_month - Duration::days(leading as i64);

    for cell_index in 0..(rows * 7) {
        let date = grid_start + Duration::days(cell_index as i64);
        let col = (cell_index % 7) as i32;
        let row = (cell_index / 7) as i32;
        let in_current_month = date.month() == month;

        let cell = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        cell.add_css_class("month-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);

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
            for event in day_events.iter().take(MAX_VISIBLE_EVENTS) {
                cell.append(&event_row(event));
            }
            if day_events.len() > MAX_VISIBLE_EVENTS {
                let more = gtk4::Label::new(Some(&format!("{} more", day_events.len() - MAX_VISIBLE_EVENTS)));
                more.set_halign(gtk4::Align::Start);
                more.add_css_class("more-label");
                cell.append(&more);
            }
        }

        grid.attach(&cell, col, row, 1, 1);
    }
}

/// One compact `<dot> <time> <title>` row for an event chip in a month cell.
fn event_row(event: &DisplayEvent) -> gtk4::Box {
    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    row_box.add_css_class("event-row");

    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("event-dot");
    if let Some(color) = &event.color {
        dot.add_css_class(&css_class_for_color(color));
    }
    row_box.append(&dot);

    let time_prefix = if event.all_day {
        String::new()
    } else {
        event.start.get(11..16).map(|s| format!("{s} ")).unwrap_or_default()
    };
    let title_label = gtk4::Label::new(Some(&format!("{time_prefix}{}", event.title)));
    title_label.set_halign(gtk4::Align::Start);
    title_label.set_hexpand(true);
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    row_box.append(&title_label);

    row_box
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("valid year/month");
    let first_of_this = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year/month");
    (first_of_next - first_of_this).num_days() as u32
}

/// The static part of the Month view's look: hairline cell borders instead of card
/// backgrounds, muted weekday labels, and a today badge in the system accent color
/// (via libadwaita's `@accent_bg_color`/`@accent_fg_color`, so it follows Omarchy's
/// theme automatically — DESIGN_SPEC.md §13).
fn load_static_css() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        "
        .month-cell {
            border-right: 1px solid alpha(currentColor, 0.12);
            border-bottom: 1px solid alpha(currentColor, 0.12);
            padding: 6px 8px;
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
        }
        .event-dot {
            min-width: 6px;
            min-height: 6px;
            margin-top: 6px;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.4);
        }
        .more-label {
            font-size: 0.8em;
            font-weight: 600;
            opacity: 0.75;
        }
        ",
    );
    add_provider(&provider);
}

/// One CSS rule per distinct calendar color actually in use, so `event_row` can tag
/// each dot with a `dot-<hex>` class instead of needing per-widget inline styling
/// (which GTK4 doesn't support from Rust the way inline HTML `style=` does).
fn load_calendar_color_css(events: &[DisplayEvent]) {
    let mut seen = HashSet::new();
    let mut css = String::new();
    for event in events {
        if let Some(color) = &event.color {
            if seen.insert(color.clone()) {
                css.push_str(&format!(".{} {{ background-color: {color}; }}\n", css_class_for_color(color)));
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

fn css_class_for_color(color: &str) -> String {
    format!("dot-{}", color.trim_start_matches('#'))
}

fn add_provider(provider: &gtk4::CssProvider) {
    let Some(display) = gtk4::gdk::Display::default() else {
        tracing::warn!("no default GDK display; skipping CSS provider registration");
        return;
    };
    gtk4::style_context_add_provider_for_display(&display, provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let core = init_core()?;

    let app = RelmApp::new("org.calendarchy.Calendarchy");
    app.run::<App>(core);

    Ok(())
}
