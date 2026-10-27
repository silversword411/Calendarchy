use super::*;
use std::cmp::Ordering;

const COLUMNS: &[(&str, &str, i32, bool)] = &[
    ("start", "Start", 180, true),
    ("end", "End", 180, true),
    ("title", "Title", 300, true),
    ("calendar", "Calendar", 160, true),
    ("location", "Location", 180, true),
    ("duration", "Duration", 100, true),
    ("kind", "Type", 90, false),
    ("guests", "Guests", 200, false),
    ("reminders", "Reminders", 100, false),
    ("recurring", "Recurring", 100, false),
    ("video", "Video call", 100, false),
    ("private", "Private", 90, false),
    ("description", "Description", 300, false),
];

fn update_settings(ctx: &EventCtx, change: impl FnOnce(&mut AppSettings), refresh: bool) {
    let mut settings = load_settings(&ctx.storage).unwrap_or_default();
    change(&mut settings);
    if let Err(err) = save_settings(&ctx.storage, &settings) {
        tracing::warn!(%err, "failed to save schedule preferences");
    }
    if refresh {
        ctx.sender.input(AppMsg::EventUpdated);
    }
}

fn column_order(settings: &AppSettings) -> Vec<String> {
    let mut result = Vec::new();
    for key in settings
        .schedule_columns
        .iter()
        .map(String::as_str)
        .chain(COLUMNS.iter().map(|c| c.0))
    {
        if COLUMNS.iter().any(|c| c.0 == key) && !result.iter().any(|k| k == key) {
            result.push(key.to_string());
        }
    }
    result
}

fn visible(key: &str, settings: &AppSettings) -> bool {
    // An explicit order means the user has customized visibility; otherwise use defaults.
    if settings.schedule_columns.is_empty() {
        COLUMNS.iter().find(|c| c.0 == key).is_some_and(|c| c.3)
    } else {
        !settings.schedule_hidden_columns.iter().any(|k| k == key)
    }
}

fn start(event: &DisplayEvent) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(&event.start)
        .ok()
        .map(|d| d.with_timezone(&Local))
}
fn event_date(event: &DisplayEvent) -> Option<NaiveDate> {
    if event.all_day {
        NaiveDate::parse_from_str(event.start_date(), "%Y-%m-%d").ok()
    } else {
        start(event).map(|d| d.date_naive())
    }
}
fn duration(event: &DisplayEvent) -> i64 {
    if event.all_day {
        let a = NaiveDate::parse_from_str(&event.start, "%Y-%m-%d");
        let b = NaiveDate::parse_from_str(&event.end, "%Y-%m-%d");
        a.ok()
            .zip(b.ok())
            .map(|(a, b)| (b - a).num_minutes())
            .unwrap_or(0)
    } else {
        DateTime::parse_from_rfc3339(&event.start)
            .ok()
            .zip(DateTime::parse_from_rfc3339(&event.end).ok())
            .map(|(a, b)| (b - a).num_minutes())
            .unwrap_or(0)
    }
}
fn text(
    event: &DisplayEvent,
    key: &str,
    date_format: DateFormat,
    time_format: TimeFormat,
) -> String {
    let yes = |v| if v { "Yes" } else { "No" }.to_string();
    match key {
        "start" | "end" => {
            let raw = if key == "start" {
                &event.start
            } else {
                &event.end
            };
            if event.all_day {
                NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                    .map(|d| format!("{} · All day", format_picker_date(d, date_format)))
                    .unwrap_or_else(|_| raw.clone())
            } else {
                format_event_timestamp(raw, date_format, time_format)
            }
        }
        "title" => event.title.clone(),
        "calendar" => event.calendar_name.clone(),
        "location" => event.location.clone().unwrap_or_default(),
        "description" => strip_description_markup(event.description.as_deref().unwrap_or("")),
        "duration" => {
            let minutes = duration(event);
            if event.all_day {
                format!("{} days", minutes / 1440)
            } else {
                format!("{}h {:02}m", minutes / 60, minutes % 60)
            }
        }
        "kind" => if event.all_day { "All day" } else { "Timed" }.into(),
        "guests" => format!(
            "{} · {}",
            event.other_attendee_count,
            event.other_attendee_names.join(", ")
        ),
        "reminders" => event.reminder_count.to_string(),
        "recurring" => yes(event.is_recurring),
        "video" => yes(event.has_video_call),
        "private" => yes(event.is_private),
        _ => String::new(),
    }
}
pub(super) fn compare(a: &DisplayEvent, b: &DisplayEvent, key: &str) -> Ordering {
    let result = match key {
        "start" | "end" => {
            let timestamp = |e: &DisplayEvent| {
                let raw = if key == "start" { &e.start } else { &e.end };
                DateTime::parse_from_rfc3339(raw)
                    .ok()
                    .map(|d| d.timestamp())
                    .or_else(|| {
                        NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                            .ok()
                            .map(|d| local_datetime(d, NaiveTime::MIN).timestamp())
                    })
            };
            timestamp(a).cmp(&timestamp(b))
        }
        "duration" => duration(a).cmp(&duration(b)),
        "reminders" => a.reminder_count.cmp(&b.reminder_count),
        "guests" => a.other_attendee_count.cmp(&b.other_attendee_count),
        _ => text(a, key, DateFormat::YearMonthDay, TimeFormat::TwentyFourHour)
            .to_lowercase()
            .cmp(
                &text(b, key, DateFormat::YearMonthDay, TimeFormat::TwentyFourHour).to_lowercase(),
            ),
    };
    result.then_with(|| a.id.cmp(&b.id))
}

pub(super) fn in_range(event: &DisplayEvent, anchor: NaiveDate, days: u32) -> bool {
    if days == 0 {
        return true;
    }
    let Some(date) = event_date(event) else {
        return false;
    };
    let end = if event.all_day {
        NaiveDate::parse_from_str(&event.end, "%Y-%m-%d")
            .ok()
            .map(|d| d - Duration::days(1))
    } else {
        DateTime::parse_from_rfc3339(&event.end)
            .ok()
            .map(|d| (d.with_timezone(&Local) - Duration::seconds(1)).date_naive())
    }
    .unwrap_or(date)
    .max(date);
    date < anchor + Duration::days(days as i64) && end >= anchor
}

pub(super) fn range_title(anchor: NaiveDate, days: u32) -> String {
    if days == 0 {
        "Table · All cached events".into()
    } else {
        date_span_title(anchor, anchor + Duration::days(days as i64 - 1))
    }
}

fn csv_cell(value: &str) -> String {
    // Spreadsheet formula escaping also applies to copied event titles/locations.
    let prefix = if value.trim_start().starts_with(['=', '+', '-', '@']) {
        "'"
    } else {
        ""
    };
    format!("\"{}{}\"", prefix, value.replace('"', "\"\""))
}

pub(super) fn populate(
    container: &gtk4::Box,
    anchor: NaiveDate,
    events: &[DisplayEvent],
    settings: &AppSettings,
    ctx: &EventCtx,
) {
    clear_children(container);
    let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    toolbar.set_margin_all(6);
    let ranges = [7, 30, 90, 365, 0];
    let range = gtk4::DropDown::from_strings(&[
        "7 days",
        "30 days",
        "90 days",
        "365 days",
        "All cached events",
    ]);
    range.set_tooltip_text(Some("Date range starting from the selected day"));
    range.set_selected(
        ranges
            .iter()
            .position(|days| *days == settings.schedule_range_days)
            .unwrap_or(1) as u32,
    );
    {
        let ctx = ctx.clone();
        range.connect_selected_notify(move |row| {
            update_settings(
                &ctx,
                |s| s.schedule_range_days = ranges[row.selected() as usize],
                true,
            );
        });
    }
    toolbar.append(&range);
    let kind = gtk4::DropDown::from_strings(&["All event types", "Timed events", "All-day events"]);
    kind.set_selected(match settings.schedule_event_kind.as_str() {
        "timed" => 1,
        "all_day" => 2,
        _ => 0,
    });
    {
        let ctx = ctx.clone();
        kind.connect_selected_notify(move |row| {
            update_settings(
                &ctx,
                |s| {
                    s.schedule_event_kind =
                        ["all", "timed", "all_day"][row.selected() as usize].into()
                },
                true,
            );
        });
    }
    toolbar.append(&kind);
    let hint = gtk4::Label::new(Some("Search with / · Click headers to sort"));
    hint.add_css_class("dim-label");
    hint.set_hexpand(true);
    hint.set_xalign(0.0);
    hint.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    toolbar.append(&hint);
    let customizer = gtk4::Button::with_label("Columns…");
    {
        let ctx = ctx.clone();
        customizer.connect_clicked(move |_| {
            let window = adw::Window::builder()
                .title("Table columns")
                .transient_for(&ctx.window)
                .modal(true)
                .default_width(420)
                .default_height(600)
                .build();
            refresh_customizer(&window, &ctx);
            window.present();
        });
    }
    toolbar.append(&customizer);
    container.append(&toolbar);

    let store = gtk4::gio::ListStore::new::<gtk4::glib::BoxedAnyObject>();
    for event in events
        .iter()
        .filter(|e| in_range(e, anchor, settings.schedule_range_days))
        .filter(|e| match settings.schedule_event_kind.as_str() {
            "timed" => !e.all_day,
            "all_day" => e.all_day,
            _ => true,
        })
    {
        store.append(&gtk4::glib::BoxedAnyObject::new(event.clone()));
    }
    let sorted = gtk4::SortListModel::new(Some(store.clone()), None::<gtk4::Sorter>);
    let selection = gtk4::SingleSelection::new(Some(sorted.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let view = gtk4::ColumnView::new(Some(selection.clone()));
    view.set_show_column_separators(true);
    view.set_show_row_separators(true);
    view.set_hexpand(true);
    view.set_vexpand(true);
    let mut columns = Vec::new();
    for key in column_order(settings) {
        let (_, title, width, _) = COLUMNS.iter().find(|c| c.0 == key).unwrap();
        let factory = gtk4::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            let item = object.downcast_ref::<gtk4::ListItem>().unwrap();
            let label = gtk4::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_margin_top(8);
            label.set_margin_bottom(8);
            item.set_child(Some(&label));
        });
        {
            let key = key.clone();
            let date_format = ctx.date_format;
            let time_format = ctx.time_format;
            factory.connect_bind(move |_, object| {
                let item = object.downcast_ref::<gtk4::ListItem>().unwrap();
                let row = item
                    .item()
                    .unwrap()
                    .downcast::<gtk4::glib::BoxedAnyObject>()
                    .unwrap();
                let event = row.borrow::<DisplayEvent>();
                let label = item.child().unwrap().downcast::<gtk4::Label>().unwrap();
                let value = text(&event, &key, date_format, time_format);
                label.set_label(&value.replace('\n', " "));
                label.set_tooltip_text(Some(&value));
                if event.self_response_status == Some(AttendeeResponseStatus::Declined) {
                    label.add_css_class("dim-label");
                } else {
                    label.remove_css_class("dim-label");
                }
            });
        }
        let column = gtk4::ColumnViewColumn::new(Some(title), Some(factory));
        column.set_resizable(true);
        column.set_fixed_width(
            settings
                .schedule_column_widths
                .get(&key)
                .copied()
                .unwrap_or(*width)
                .clamp(60, 1200),
        );
        column.set_visible(visible(&key, settings));
        {
            let key = key.clone();
            column.set_sorter(Some(&gtk4::CustomSorter::new(move |a, b| {
                let a = a
                    .downcast_ref::<gtk4::glib::BoxedAnyObject>()
                    .unwrap()
                    .borrow::<DisplayEvent>();
                let b = b
                    .downcast_ref::<gtk4::glib::BoxedAnyObject>()
                    .unwrap()
                    .borrow::<DisplayEvent>();
                match compare(&a, &b, &key) {
                    Ordering::Less => gtk4::Ordering::Smaller,
                    Ordering::Equal => gtk4::Ordering::Equal,
                    Ordering::Greater => gtk4::Ordering::Larger,
                }
            })));
        }
        {
            let key = key.clone();
            let ctx = ctx.clone();
            column.connect_fixed_width_notify(move |column| {
                update_settings(
                    &ctx,
                    |s| {
                        s.schedule_column_widths
                            .insert(key.clone(), column.fixed_width());
                    },
                    false,
                );
            });
        }
        view.append_column(&column);
        columns.push((key, column));
    }
    sorted.set_sorter(view.sorter().as_ref());
    let sort_column = columns
        .iter()
        .find(|(key, _)| key == &settings.schedule_sort_column)
        .unwrap_or(&columns[0]);
    view.sort_by_column(
        Some(&sort_column.1),
        if settings.schedule_sort_descending {
            gtk4::SortType::Descending
        } else {
            gtk4::SortType::Ascending
        },
    );
    if let Some(sorter) = view.sorter().and_downcast::<gtk4::ColumnViewSorter>() {
        let ctx = ctx.clone();
        sorter.connect_changed(move |sorter, _| {
            if let Some(column) = sorter.primary_sort_column() {
                if let Some((key, _)) = columns.iter().find(|(_, c)| *c == column) {
                    update_settings(
                        &ctx,
                        |s| {
                            s.schedule_sort_column = key.clone();
                            s.schedule_sort_descending =
                                sorter.primary_sort_order() == gtk4::SortType::Descending;
                        },
                        false,
                    );
                }
            }
        });
    }
    {
        let ctx = ctx.clone();
        let sorted = sorted.clone();
        view.connect_activate(move |_, position| {
            if let Some(row) = sorted
                .item(position)
                .and_downcast::<gtk4::glib::BoxedAnyObject>()
            {
                open_event_editor(row.borrow::<DisplayEvent>().id, &ctx);
            }
        });
    }
    let scroller = gtk4::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&view)
        .build();
    container.append(&scroller);
    let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    footer.set_margin_all(6);
    let count = gtk4::Label::new(Some(&format!(
        "{} events · Visible calendars · Double-click a row to edit",
        store.n_items()
    )));
    count.set_hexpand(true);
    count.set_xalign(0.0);
    count.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    footer.append(&count);
    let edit = gtk4::Button::with_label("Edit selected");
    edit.set_sensitive(false);
    {
        let edit_weak = edit.downgrade();
        selection.connect_selected_notify(move |s| {
            if let Some(edit) = edit_weak.upgrade() {
                edit.set_sensitive(s.selected_item().is_some());
            }
        });
        let selection = selection.clone();
        let ctx = ctx.clone();
        edit.connect_clicked(move |_| {
            if let Some(row) = selection
                .selected_item()
                .and_downcast::<gtk4::glib::BoxedAnyObject>()
            {
                open_event_editor(row.borrow::<DisplayEvent>().id, &ctx);
            }
        });
    }
    footer.append(&edit);
    let copy = gtk4::Button::with_label("Copy CSV");
    copy.set_tooltip_text(Some(
        "Copy visible columns and filtered rows in current sort order",
    ));
    {
        let keys: Vec<String> = column_order(settings)
            .into_iter()
            .filter(|k| visible(k, settings))
            .collect();
        let date_format = ctx.date_format;
        let time_format = ctx.time_format;
        copy.connect_clicked(move |button| {
            let header = keys
                .iter()
                .map(|k| csv_cell(COLUMNS.iter().find(|c| c.0 == k).unwrap().1))
                .collect::<Vec<_>>()
                .join(",");
            let mut lines = vec![header];
            for index in 0..sorted.n_items() {
                let row = sorted
                    .item(index)
                    .unwrap()
                    .downcast::<gtk4::glib::BoxedAnyObject>()
                    .unwrap();
                let event = row.borrow::<DisplayEvent>();
                lines.push(
                    keys.iter()
                        .map(|key| csv_cell(&text(&event, key, date_format, time_format)))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            button.clipboard().set_text(&lines.join("\r\n"));
            button.set_tooltip_text(Some("Copied table as CSV"));
        });
    }
    footer.append(&copy);
    if store.n_items() == 0 {
        count.set_label("No matching events · Change the date range, search, or visible calendars");
    }
    container.append(&footer);
}

fn refresh_customizer(window: &adw::Window, ctx: &EventCtx) {
    let settings = load_settings(&ctx.storage).unwrap_or_default();
    window.set_content(Some(&column_customizer(&settings, ctx, window)));
}

fn column_customizer(settings: &AppSettings, ctx: &EventCtx, window: &adw::Window) -> gtk4::Box {
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    content.set_margin_all(12);
    content.append(&gtk4::Label::new(Some("Columns · Show, hide, and reorder")));
    let order = column_order(settings);
    let visible_count = order.iter().filter(|k| visible(k, settings)).count();
    for (index, key) in order.iter().enumerate() {
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let check = gtk4::CheckButton::with_label(COLUMNS.iter().find(|c| c.0 == key).unwrap().1);
        check.set_hexpand(true);
        check.set_active(visible(key, settings));
        check.set_sensitive(!(visible_count == 1 && check.is_active()));
        {
            let key = key.clone();
            let order = order.clone();
            let hidden: Vec<String> = order
                .iter()
                .filter(|k| !visible(k, settings))
                .cloned()
                .collect();
            let ctx = ctx.clone();
            let window = window.downgrade();
            check.connect_toggled(move |check| {
                update_settings(
                    &ctx,
                    |s| {
                        s.schedule_columns = order.clone();
                        s.schedule_hidden_columns = hidden.clone();
                        s.schedule_hidden_columns.retain(|k| k != &key);
                        if !check.is_active() {
                            s.schedule_hidden_columns.push(key.clone());
                        }
                    },
                    true,
                );
                if let Some(window) = window.upgrade() {
                    refresh_customizer(&window, &ctx);
                }
            });
        }
        row.append(&check);
        for (icon, delta, label) in [
            ("go-up-symbolic", -1, "Move column left"),
            ("go-down-symbolic", 1, "Move column right"),
        ] {
            let button = gtk4::Button::from_icon_name(icon);
            button.add_css_class("flat");
            button.set_tooltip_text(Some(label));
            let target = index as isize + delta;
            button.set_sensitive(target >= 0 && target < order.len() as isize);
            let mut order = order.clone();
            let hidden: Vec<String> = order
                .iter()
                .filter(|k| !visible(k, settings))
                .cloned()
                .collect();
            if target >= 0 && target < order.len() as isize {
                order.swap(index, target as usize);
            }
            let ctx = ctx.clone();
            let window = window.downgrade();
            button.connect_clicked(move |_| {
                update_settings(
                    &ctx,
                    |s| {
                        s.schedule_columns = order.clone();
                        s.schedule_hidden_columns = hidden.clone();
                    },
                    true,
                );
                if let Some(window) = window.upgrade() {
                    refresh_customizer(&window, &ctx);
                }
            });
            row.append(&button);
        }
        content.append(&row);
    }
    let reset = gtk4::Button::with_label("Reset table layout");
    let ctx = ctx.clone();
    let window_weak = window.downgrade();
    reset.connect_clicked(move |_| {
        update_settings(
            &ctx,
            |s| {
                s.schedule_columns.clear();
                s.schedule_hidden_columns.clear();
                s.schedule_column_widths.clear();
                s.schedule_sort_column = "start".into();
                s.schedule_sort_descending = false;
            },
            true,
        );
        if let Some(window) = window_weak.upgrade() {
            refresh_customizer(&window, &ctx);
        }
    });
    content.append(&reset);
    let scroller = gtk4::ScrolledWindow::builder()
        .max_content_height(560)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&content)
        .build();
    let layout = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    layout.append(&header);
    scroller.set_vexpand(true);
    layout.append(&scroller);
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start: &str, end: &str, all_day: bool) -> DisplayEvent {
        DisplayEvent {
            id: 1,
            title: "Event".into(),
            description: None,
            location: None,
            start: start.into(),
            end: end.into(),
            all_day,
            color: None,
            calendar_name: "Work".into(),
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
    fn range_includes_overlapping_events_and_excludes_exclusive_end() {
        let anchor = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        assert!(in_range(
            &event("2026-10-26", "2026-10-28", true),
            anchor,
            7
        ));
        assert!(!in_range(
            &event("2026-10-26", "2026-10-27", true),
            anchor,
            7
        ));
        assert!(!in_range(
            &event("2026-11-03", "2026-11-04", true),
            anchor,
            7
        ));
        assert!(in_range(
            &event("2020-01-01", "2020-01-02", true),
            anchor,
            0
        ));
    }

    #[test]
    fn sort_uses_actual_timestamps_and_numeric_durations() {
        let earlier = event(
            "2026-10-27T12:00:00+02:00",
            "2026-10-27T12:10:00+02:00",
            false,
        );
        let later = event(
            "2026-10-27T11:00:00+00:00",
            "2026-10-27T12:00:00+00:00",
            false,
        );
        assert_eq!(compare(&earlier, &later, "start"), Ordering::Less);
        assert_eq!(compare(&earlier, &later, "duration"), Ordering::Less);
    }

    #[test]
    fn layout_deduplicates_unknown_columns_and_keeps_new_columns_available() {
        let mut settings = AppSettings::default();
        settings.schedule_columns = vec!["title".into(), "unknown".into(), "title".into()];
        settings.schedule_hidden_columns = vec!["location".into()];
        let order = column_order(&settings);
        assert_eq!(order[0], "title");
        assert_eq!(order.len(), COLUMNS.len());
        assert!(!visible("location", &settings));
        assert!(visible("start", &settings));
    }

    #[test]
    fn csv_quotes_multiline_values_and_escapes_spreadsheet_formulas() {
        assert_eq!(csv_cell("A, \"B\"\nC"), "\"A, \"\"B\"\"\nC\"");
        assert_eq!(csv_cell("=SUM(1,2)"), "\"'=SUM(1,2)\"");
    }

    #[test]
    fn schedule_settings_survive_storage_round_trip() {
        let storage = Storage::open_in_memory().unwrap();
        let mut settings = AppSettings::default();
        settings.last_view_mode = LastViewMode::Table;
        settings.schedule_columns = vec!["title".into(), "start".into()];
        settings.schedule_hidden_columns = vec!["description".into()];
        settings.schedule_column_widths.insert("title".into(), 480);
        settings.schedule_sort_column = "duration".into();
        settings.schedule_sort_descending = true;
        settings.schedule_range_days = 90;
        settings.schedule_event_kind = "timed".into();
        save_settings(&storage, &settings).unwrap();
        assert_eq!(load_settings(&storage).unwrap(), settings);
    }
}
