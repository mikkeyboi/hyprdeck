//! Application shell: single-instance GApplication, sidebar window with lazily
//! built pages, tray host and the app event loop.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::ui::{Ctx, PageInfo};
use hyprdeck_core::{APP_ICON, APP_ID, rt};

use crate::prefs;

/// Options understood by the GUI (also when forwarded from a second instance).
#[derive(Debug, Default)]
struct GuiArgs {
    /// Started by the login unit: honour the "start minimized" preference.
    background: bool,
    /// Never open the window.
    minimized: bool,
    page: Option<String>,
}

impl GuiArgs {
    fn parse(args: &[String]) -> GuiArgs {
        let mut out = GuiArgs::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--background" => out.background = true,
                "--minimized" => out.minimized = true,
                "--page" => out.page = it.next().cloned(),
                other => tracing::warn!("ignoring unknown argument {other:?}"),
            }
        }
        out
    }
}

fn all_pages() -> Vec<PageInfo> {
    let mut pages = Vec::new();
    pages.extend(hd_startup::pages());
    pages.extend(hd_display::pages());
    pages.extend(hd_input::pages());
    pages.extend(hd_audio::pages());
    pages.extend(hd_bluetooth::pages());
    pages.extend(hd_defaults::pages());
    pages.extend(hd_updates::pages());
    pages.extend(hd_system::pages());
    pages
}

fn start_background_services() {
    // Migrate retired notification switches before providers can save their settings.
    hyprdeck_core::notify::settings();
    hd_startup::start_background();
    hd_display::start_background();
    hd_input::start_background();
    hd_audio::start_background();
    hd_bluetooth::start_background();
    hd_defaults::start_background();
    hd_updates::start_background();
    hd_system::start_background();
}

struct Shell {
    ctx: Ctx,
    pages: Vec<PageInfo>,
    sidebar: gtk::ListBox,
}

impl Shell {
    fn build(app: &adw::Application, tray_ok: Rc<Cell<bool>>) -> Rc<Shell> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Hyprdeck")
            .icon_name(APP_ICON)
            .default_width(1120)
            .default_height(780)
            .build();
        let toasts = adw::ToastOverlay::new();
        let ctx = Ctx {
            window: window.clone(),
            toasts: toasts.clone(),
        };
        let pages = all_pages();

        // Sidebar.
        let sidebar = gtk::ListBox::new();
        sidebar.add_css_class("navigation-sidebar");
        for p in &pages {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.set_margin_top(6);
            row.set_margin_bottom(6);
            row.set_margin_start(6);
            row.append(&gtk::Image::from_icon_name(p.icon));
            row.append(&gtk::Label::builder().label(p.title).xalign(0.0).build());
            sidebar.append(&row);
        }
        let menu = gio::Menu::new();
        menu.append(Some("Preferences"), Some("app.preferences"));
        menu.append(Some("About Hyprdeck"), Some("app.about"));
        menu.append(Some("Quit"), Some("app.quit"));
        let side_header = adw::HeaderBar::new();
        side_header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .build(),
        );
        let side_view = adw::ToolbarView::new();
        side_view.add_top_bar(&side_header);
        side_view.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&sidebar)
                .build(),
        ));

        // Content.
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let title = adw::WindowTitle::new("", "");
        let content_header = adw::HeaderBar::new();
        content_header.set_title_widget(Some(&title));
        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&content_header);
        content_view.set_content(Some(&stack));

        let split = adw::NavigationSplitView::builder()
            .sidebar(&adw::NavigationPage::new(&side_view, "Hyprdeck"))
            .content(&adw::NavigationPage::new(&content_view, "Hyprdeck"))
            .min_sidebar_width(220.0)
            .build();
        let bp = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 640sp").expect("valid condition"),
        );
        bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(bp);
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        // Lazy page construction on selection.
        let built: Rc<RefCell<Vec<&'static str>>> = Rc::default();
        {
            let pages = pages.clone();
            let ctx = ctx.clone();
            let split = split.clone();
            sidebar.connect_row_selected(move |_, row| {
                let Some(row) = row else { return };
                let Some(p) = pages.get(row.index() as usize) else {
                    return;
                };
                if !built.borrow().contains(&p.id) {
                    let widget = (p.build)(&ctx);
                    stack.add_named(&widget, Some(p.id));
                    built.borrow_mut().push(p.id);
                }
                stack.set_visible_child_name(p.id);
                title.set_title(p.title);
                if let Some(content) = split.content() {
                    content.set_title(p.title);
                }
                split.set_show_content(true);
            });
        }

        // Close → hide to tray (when available and enabled) or quit.
        {
            let app = app.clone();
            window.connect_close_request(move |w| {
                if tray_ok.get() && prefs::load().close_to_tray {
                    w.set_visible(false);
                } else {
                    app.quit();
                }
                glib::Propagation::Stop
            });
        }
        window.connect_visible_notify(|w| hyprdeck_core::ui::set_window_visible(w.is_visible()));

        Rc::new(Shell {
            ctx,
            pages,
            sidebar,
        })
    }

    fn present(&self, page: Option<&str>) {
        let index = page
            .and_then(|id| self.pages.iter().position(|p| p.id == id))
            .or_else(|| self.sidebar.selected_row().is_none().then_some(0));
        if let Some(i) = index
            && let Some(row) = self.sidebar.row_at_index(i as i32)
        {
            self.sidebar.select_row(Some(&row));
        }
        self.ctx.window.set_visible(true);
        self.ctx.window.present();
    }
}

pub fn run() -> glib::ExitCode {
    // The Wayland app_id comes from the program name; under an AppImage that is
    // `AppRun.wrapped`, which breaks the desktop-file/icon association.
    glib::set_prgname(Some(APP_ID));
    glib::set_application_name("Hyprdeck");
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let shell: Rc<RefCell<Option<Rc<Shell>>>> = Rc::default();
    let tray_ok = Rc::new(Cell::new(false));
    let started = Rc::new(Cell::new(false));
    let hold: Rc<RefCell<Option<gio::ApplicationHoldGuard>>> = Rc::default();

    let get_shell = {
        let shell = shell.clone();
        let tray_ok = tray_ok.clone();
        move |app: &adw::Application| -> Rc<Shell> {
            shell
                .borrow_mut()
                .get_or_insert_with(|| Shell::build(app, tray_ok.clone()))
                .clone()
        }
    };

    install_actions(&app, get_shell.clone(), hold.clone());

    app.connect_command_line(move |app, cl| {
        let args: Vec<String> = cl
            .arguments()
            .iter()
            .skip(1)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let opts = GuiArgs::parse(&args);
        let first = !started.replace(true);
        if first {
            *hold.borrow_mut() = Some(app.hold());
            start_background_services();
            event_loop(app, get_shell.clone(), hold.clone());
        }
        let show =
            !opts.minimized && (!opts.background || !first || !prefs::load().start_minimized);
        if first {
            // Register the tray, then decide visibility (no tray → always show).
            let app = app.clone();
            let tray_ok = tray_ok.clone();
            let get_shell = get_shell.clone();
            glib::spawn_future_local(async move {
                let ok = rt::run(crate::tray::run()).await;
                tray_ok.set(ok);
                if show || !ok {
                    get_shell(&app).present(opts.page.as_deref());
                }
            });
        } else if show {
            get_shell(app).present(opts.page.as_deref());
        }
        glib::ExitCode::SUCCESS
    });

    app.run()
}

fn install_actions(
    app: &adw::Application,
    get_shell: impl Fn(&adw::Application) -> Rc<Shell> + Clone + 'static,
    hold: Rc<RefCell<Option<gio::ApplicationHoldGuard>>>,
) {
    let prefs_action = gio::SimpleAction::new("preferences", None);
    {
        let app = app.clone();
        let get_shell = get_shell.clone();
        prefs_action.connect_activate(move |_, _| prefs::show_dialog(&get_shell(&app).ctx));
    }
    let about = gio::SimpleAction::new("about", None);
    {
        let app = app.clone();
        about.connect_activate(move |_, _| {
            let dialog = adw::AboutDialog::builder()
                .application_name("Hyprdeck")
                .application_icon(APP_ICON)
                .version(hyprdeck_core::version_string())
                .comments("Startup apps, displays, input, audio, Bluetooth, updates and sleep/wake for Hyprland + Noctalia")
                .developer_name("mikkeyboi")
                .website("https://github.com/mikkeyboi/hyprdeck")
                .issue_url("https://github.com/mikkeyboi/hyprdeck/issues")
                .license_type(gtk::License::MitX11)
                .build();
            dialog.present(Some(&get_shell(&app).ctx.window));
        });
    }
    let quit = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        quit.connect_activate(move |_, _| {
            hold.borrow_mut().take();
            app.quit();
        });
    }
    app.add_action(&prefs_action);
    app.add_action(&about);
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
}

fn event_loop(
    app: &adw::Application,
    get_shell: impl Fn(&adw::Application) -> Rc<Shell> + 'static,
    hold: Rc<RefCell<Option<gio::ApplicationHoldGuard>>>,
) {
    let app = app.clone();
    let rx = events::receiver();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                AppEvent::ShowWindow => get_shell(&app).present(None),
                AppEvent::ShowPage(id) => get_shell(&app).present(Some(&id)),
                AppEvent::Toast(msg) if hyprdeck_core::ui::window_visible() => {
                    get_shell(&app).ctx.toast(msg);
                }
                AppEvent::Toast(_) => {}
                AppEvent::Quit => {
                    hold.borrow_mut().take();
                    app.quit();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::GuiArgs;

    #[test]
    fn parses_gui_flags() {
        let a = GuiArgs::parse(&["--background".into(), "--page".into(), "audio".into()]);
        assert!(a.background && !a.minimized);
        assert_eq!(a.page.as_deref(), Some("audio"));
    }
}
