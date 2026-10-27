use super::*;

/// Fixed forward-looking pages match the sidebar's selected date. Empty days are
/// omitted, while events crossing midnight appear under each day they occupy.
fn grouped_events(
    anchor: NaiveDate,
    events: &[DisplayEvent],
) -> Vec<(NaiveDate, Vec<DisplayEvent>)> {
    (0..30)
        .filter_map(|offset| {
            let date = anchor + Duration::days(offset);
            let mut rows: Vec<_> = events
                .iter()
                .filter(|event| table::in_range(event, date, 1))
                .cloned()
                .collect();
            rows.sort_by(|a, b| {
                b.all_day
                    .cmp(&a.all_day)
                    .then_with(|| table::compare(a, b, "start"))
            });
            if rows.is_empty() {
                None
            } else {
                Some((date, rows))
            }
        })
        .collect()
}

pub(super) fn populate(
    container: &gtk4::Box,
    anchor: NaiveDate,
    events: &[DisplayEvent],
    ctx: &EventCtx,
) {
    clear_children(container);
    let scroller = gtk4::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    scroller.add_css_class("card");
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.set_margin_all(16);
    let groups = grouped_events(anchor, events);
    if groups.is_empty() {
        let empty = gtk4::Label::new(Some("No events in the next 30 days\nChange the selected date, search, or visible calendars."));
        empty.add_css_class("dim-label");
        empty.set_margin_all(32);
        content.append(&empty);
    }
    let today = Local::now().date_naive();
    for (date, events) in groups {
        let day = gtk4::Box::new(gtk4::Orientation::Horizontal, 20);
        day.set_margin_top(12);
        day.set_margin_bottom(12);
        let date_button = gtk4::Button::new();
        date_button.add_css_class("flat");
        date_button.add_css_class("schedule-date");
        date_button.set_width_request(160);
        date_button.set_valign(gtk4::Align::Start);
        date_button.set_tooltip_text(Some("Open day view"));
        let date_label = gtk4::Box::new(gtk4::Orientation::Horizontal, 18);
        let number = gtk4::Label::new(Some(&date.day().to_string()));
        number.add_css_class("schedule-date-number");
        number.set_width_chars(2);
        number.set_xalign(0.5);
        let month_weekday = gtk4::Label::new(Some(&date.format("%b, %a").to_string().to_uppercase()));
        month_weekday.add_css_class("schedule-date-detail");
        month_weekday.set_xalign(0.0);
        date_label.append(&number);
        date_label.append(&month_weekday);
        date_button.set_child(Some(&date_label));
        if date == today {
            date_button.add_css_class("schedule-date-today");
        }
        {
            let sender = ctx.sender.clone();
            date_button.connect_clicked(move |_| {
                sender.input(AppMsg::JumpToDate(date));
                sender.input(AppMsg::SetView(ViewMode::Day));
            });
        }
        day.append(&date_button);
        let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        rows.set_hexpand(true);
        for event in events {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            row.add_css_class("event-row");
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_cursor_from_name(Some("pointer"));
            if event.self_response_status == Some(AttendeeResponseStatus::Declined) {
                row.add_css_class("event-declined");
            }
            let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            dot.add_css_class("event-dot");
            dot.set_valign(gtk4::Align::Center);
            if let Some(color) = &event.color {
                dot.add_css_class(&css_class_for_color(color));
            }
            row.append(&dot);
            let time = gtk4::Label::new(Some(&time_range(&event, ctx.time_format)));
            time.set_width_chars(22);
            time.set_xalign(0.0);
            row.append(&time);
            let details = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
            details.set_hexpand(true);
            let title = gtk4::Label::new(Some(&event.title));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            details.append(&title);
            if let Some(location) = event.location.as_deref().filter(|s| !s.is_empty()) {
                let label = gtk4::Label::new(Some(location));
                label.set_xalign(0.0);
                label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                label.add_css_class("dim-label");
                details.append(&label);
            }
            row.append(&details);
            if let Some(badges) = event_badge_row(&event) {
                row.append(&badges);
            }
            let calendar = gtk4::Label::new(Some(&event.calendar_name));
            calendar.add_css_class("dim-label");
            calendar.set_max_width_chars(20);
            calendar.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            row.append(&calendar);
            row.set_tooltip_text(Some(&format!(
                "{}\n{}\n{}",
                event.title,
                event.calendar_name,
                strip_description_markup(event.description.as_deref().unwrap_or(""))
            )));
            wire_event_click(&row, &event, ctx);
            rows.append(&row);
        }
        day.append(&rows);
        content.append(&day);
        content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    }
    scroller.set_child(Some(&content));
    container.append(&scroller);
}

fn time_range(event: &DisplayEvent, format: TimeFormat) -> String {
    if event.all_day {
        return "All day".into();
    }
    let clock = |raw: &str| {
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|dt| format_clock(dt.with_timezone(&Local).time(), format))
            .unwrap_or_default()
    };
    format!("{} – {}", clock(&event.start), clock(&event.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schedule_groups_multiday_events_and_omits_empty_days() {
        let anchor = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let event = DisplayEvent {
            id: 1,
            title: "Trip".into(),
            start: "2026-10-27".into(),
            end: "2026-10-29".into(),
            all_day: true,
            description: None,
            location: None,
            color: None,
            calendar_name: "Work".into(),
            is_recurring: false,
            has_video_call: false,
            is_private: false,
            self_response_status: None,
            reminder_count: 0,
            other_attendee_count: 0,
            other_attendee_names: vec![],
            attachment_count: 0,
        };
        let groups = grouped_events(anchor, &[event]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, anchor);
        assert_eq!(groups[1].0, anchor + Duration::days(1));
        assert_eq!(groups[0].1[0].title, "Trip");
    }
}
