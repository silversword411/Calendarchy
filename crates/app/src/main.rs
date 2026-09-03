use std::sync::Arc;

use calendarchy_core::{AccountManager, InMemoryKeyring, ServiceRegistry, Storage};
use calendarchy_service_calendar::CalendarService;
use gtk4::prelude::*;
use relm4::adw;
use relm4::prelude::*;

/// Core wiring (DESIGN_SPEC.md §6, roadmap phase 0) plumbed into the app so the UI
/// layer never talks to storage/services directly — it goes through this handle.
/// Real calendar views land in later roadmap phases; this window just proves the
/// GTK4/libadwaita + relm4 stack runs on this machine on top of the core foundation.
struct AppCore {
    accounts: AccountManager,
}

fn init_core() -> anyhow::Result<AppCore> {
    let storage = Storage::open_in_memory()?;

    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(CalendarService::new()));
    let registry = Arc::new(registry);

    storage.with_conn(|conn| registry.migrate_all(conn))?;

    let keyring = Arc::new(InMemoryKeyring::new());
    let accounts = AccountManager::new(storage, registry, keyring);

    Ok(AppCore { accounts })
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
            set_default_width: 900,
            set_default_height: 640,

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &adw::StatusPage {
                    set_icon_name: Some("x-office-calendar-symbolic"),
                    set_title: "Calendarchy",
                    set_description: Some("No accounts connected yet."),
                },
            }
        }
    }

    fn init(
        core: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let accounts = core.accounts.list_accounts().unwrap_or_default();
        tracing::info!(count = accounts.len(), "loaded accounts at startup");

        let model = App { core };
        let widgets = view_output!();
        let _ = sender;
        let _ = &model.core;
        ComponentParts { model, widgets }
    }

    fn update(&mut self, _msg: Self::Input, _sender: ComponentSender<Self>) {}
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let core = init_core()?;

    let app = RelmApp::new("org.calendarchy.Calendarchy");
    app.run::<App>(core);

    Ok(())
}
