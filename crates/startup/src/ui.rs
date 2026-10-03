//! The "Startup Apps" page.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use anyhow::{Result, anyhow};
use gtk::{gdk, gio, glib};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::cleanup::{self, tilde};
use crate::entries::{self, Entry, Section, Snapshot, State, Toggled};
use crate::{desktop, units, xdg};

const CSS: &str = "
.hd-pill { border-radius: 999px; padding: 2px 10px; font-size: 0.8em; font-weight: 600; }
.hd-pill.running { color: @success_color; background: alpha(@success_color, 0.15); }
.hd-pill.failed { color: @error_color; background: alpha(@error_color, 0.15); }
.hd-pill.disabled { color: @warning_color; background: alpha(@warning_color, 0.15); }
.hd-pill.neutral { background: alpha(currentColor, 0.1); }
";

fn install_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

struct Group {
    group: adw::PreferencesGroup,
    children: RefCell<Vec<gtk::Widget>>,
}

impl Group {
    fn new(title: &str, description: &str) -> Self {
        Self {
            group: adw::PreferencesGroup::builder()
                .title(title)
                .description(description)
                .build(),
            children: RefCell::default(),
        }
    }
    fn add(&self, w: &impl IsA<gtk::Widget>) {
        self.group.add(w);
        self.children.borrow_mut().push(w.clone().upcast());
    }
    fn clear(&self) {
        for w in self.children.borrow_mut().drain(..) {
            self.group.remove(&w);
        }
    }
}

struct Page {
    ctx: Ctx,
    search: gtk::SearchEntry,
    spinner: adw::Spinner,
    banner: adw::Banner,
    apps: Group,
    services: Group,
    compositor: Group,
    no_match: adw::StatusPage,
    generation: Cell<u64>,
    rows: RefCell<Vec<(adw::ActionRow, Rc<Entry>)>>,
    expanders: RefCell<Vec<(adw::ExpanderRow, Vec<adw::ActionRow>)>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    install_css();
    let (scroller, content) = ui::page_scaffold();

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search startup items")
        .hexpand(true)
        .build();
    let spinner = adw::Spinner::builder().visible(false).build();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reload all startup items")
        .build();
    let add = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("list-add-symbolic")
                .label("Add startup app")
                .build(),
        )
        .css_classes(["suggested-action"])
        .build();
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.append(&search);
    top.append(&spinner);
    top.append(&refresh);
    top.append(&add);
    content.append(&top);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        search,
        spinner,
        banner: adw::Banner::builder()
            .use_markup(false)
            .revealed(false)
            .build(),
        apps: Group::new(
            "Apps",
            "Desktop entries in ~/.config/autostart and /etc/xdg/autostart, started at login through systemd's XDG autostart support.",
        ),
        services: Group::new(
            "Background services",
            "systemd user services. The switch controls starting at login; the menu controls the running service.",
        ),
        compositor: Group::new(
            "Compositor",
            "Commands Hyprland runs at startup (hl.on(\"hyprland.start\") in your Lua config).",
        ),
        no_match: adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("No matching startup items")
            .visible(false)
            .build(),
        generation: Cell::new(0),
        rows: RefCell::default(),
        expanders: RefCell::default(),
    });
    content.append(&page.apps.group);
    content.append(&page.services.group);
    content.append(&page.compositor.group);
    content.append(&page.no_match);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&page.banner);
    outer.append(&scroller);

    let p = page.clone();
    page.search
        .connect_search_changed(move |_| p.apply_filter());
    let p = page.clone();
    refresh.connect_clicked(move |_| p.refresh());
    let p = page.clone();
    add.connect_clicked(move |_| p.add_dialog());
    let p = page.clone();
    ui::on_shown(&outer, move || p.refresh());
    outer.upcast()
}

/// Index into `[apps, services, compositor]`.
fn group_index(s: Section) -> usize {
    match s {
        Section::Apps | Section::AppsSkipped => 0,
        Section::Services | Section::VendorServices => 1,
        Section::Compositor => 2,
    }
}

fn pill(state: State) -> gtk::Label {
    let class = match state {
        State::Running | State::Waiting => "running",
        State::Failed => "failed",
        State::Disabled => "disabled",
        State::Stopped | State::Skipped => "neutral",
    };
    gtk::Label::builder()
        .label(state.label())
        .css_classes(["hd-pill", class])
        .valign(gtk::Align::Center)
        .build()
}

fn icon_for(e: &Entry) -> gtk::Image {
    let fallback = match e {
        Entry::App(..) => "application-x-executable-symbolic",
        Entry::Service(_) => "emblem-system-symbolic",
        Entry::Compositor(_) => "utilities-terminal-symbolic",
    };
    let theme = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
    let img = match e.icon() {
        Some(i) if i.starts_with('/') && Path::new(i).is_file() => gtk::Image::from_file(i),
        Some(i) if theme.is_some_and(|t| t.has_icon(i)) => gtk::Image::from_icon_name(i),
        _ => gtk::Image::from_icon_name(fallback),
    };
    img.set_pixel_size(32);
    img
}

fn tooltip(e: &Entry) -> String {
    let mut lines = vec![e.detail()];
    match e {
        Entry::App(a, _) => {
            lines.push(format!("Command: {}", a.command));
            lines.push(format!("File: {}", tilde(&a.path)));
            if a.user_path.is_some() && a.system_path.is_some() {
                lines.push("Your copy overrides the system entry".into());
            }
        }
        Entry::Service(u) => {
            if let Some(x) = &u.exec {
                lines.push(format!("Runs: {x}"));
            }
            lines.push(format!("File: {}", tilde(&u.path)));
        }
        Entry::Compositor(h) => lines.push(format!("Defined at {}", h.location())),
    }
    if let Some(why) = e.toggle_blocker() {
        lines.push(why);
    }
    lines.join("\n")
}

impl Page {
    fn refresh(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.spinner.set_visible(true);
        let page = self.clone();
        glib::spawn_future_local(async move {
            let snap = rt::blocking(entries::load).await;
            if page.generation.get() != generation {
                return;
            }
            page.spinner.set_visible(false);
            page.populate(snap);
        });
    }

    fn populate(self: &Rc<Self>, snap: Snapshot) {
        self.apps.clear();
        self.services.clear();
        self.compositor.clear();
        self.rows.borrow_mut().clear();
        self.expanders.borrow_mut().clear();

        if snap.errors.is_empty() {
            self.banner.set_revealed(false);
        } else {
            self.banner.set_title(&snap.errors.join(" · "));
            self.banner.set_revealed(true);
        }

        let entries: Vec<Rc<Entry>> = snap.entries.into_iter().map(Rc::new).collect();
        let in_section = |s: Section| entries.iter().filter(move |e| e.section() == s);

        for e in in_section(Section::Apps) {
            self.apps.add(&self.entry_row(e));
        }
        self.add_expander(
            &self.apps,
            "Not used on Hyprland",
            "Entries for other desktops, skipped by systemd, or whose program is missing",
            in_section(Section::AppsSkipped),
        );
        for e in in_section(Section::Services) {
            self.services.add(&self.entry_row(e));
        }
        self.add_expander(
            &self.services,
            "System-provided",
            "Enabled user services shipped by installed packages",
            in_section(Section::VendorServices),
        );
        for e in in_section(Section::Compositor) {
            self.compositor.add(&self.entry_row(e));
        }
        for g in [&self.apps, &self.services, &self.compositor] {
            if g.children.borrow().is_empty() {
                g.add(
                    &adw::ActionRow::builder()
                        .title("Nothing here")
                        .css_classes(["dim-label"])
                        .build(),
                );
            }
        }
        self.apply_filter();
    }

    fn add_expander<'a>(
        self: &Rc<Self>,
        group: &Group,
        title: &str,
        subtitle: &str,
        items: impl Iterator<Item = &'a Rc<Entry>>,
    ) {
        let exp = adw::ExpanderRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        let mut children = Vec::new();
        for e in items {
            let row = self.entry_row(e);
            exp.add_row(&row);
            children.push(row);
        }
        if children.is_empty() {
            return;
        }
        exp.add_suffix(
            &gtk::Label::builder()
                .label(children.len().to_string())
                .css_classes(["dim-label"])
                .build(),
        );
        group.add(&exp);
        self.expanders.borrow_mut().push((exp, children));
    }

    fn apply_filter(&self) {
        let q = self.search.text().trim().to_lowercase();
        let mut shown = [false; 3];
        for (row, e) in self.rows.borrow().iter() {
            let visible = e.matches(&q);
            row.set_visible(visible);
            shown[group_index(e.section())] |= visible;
        }
        for (exp, children) in self.expanders.borrow().iter() {
            // `get_visible`: the row's own flag (`is_visible` also checks the hidden expander).
            let any = children.iter().any(|r| r.get_visible());
            exp.set_visible(any);
            if !q.is_empty() && any {
                exp.set_expanded(true);
            }
        }
        for (g, any) in [&self.apps, &self.services, &self.compositor]
            .into_iter()
            .zip(shown)
        {
            g.group.set_visible(q.is_empty() || any);
        }
        self.no_match
            .set_visible(!q.is_empty() && !shown.contains(&true));
    }

    fn entry_row(self: &Rc<Self>, e: &Rc<Entry>) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title_lines(1)
            .subtitle_lines(1)
            .tooltip_text(tooltip(e))
            .build();
        row.set_title(&e.name());
        row.set_subtitle(&e.detail());
        row.add_prefix(&icon_for(e));
        row.add_suffix(&pill(e.state()));

        let sw = gtk::Switch::builder()
            .active(e.enabled())
            .valign(gtk::Align::Center)
            .build();
        match e.toggle_blocker() {
            Some(why) => {
                sw.set_sensitive(false);
                sw.set_tooltip_text(Some(&why));
            }
            None => sw.set_tooltip_text(Some("Start at next login")),
        }
        let (page, entry) = (self.clone(), e.clone());
        sw.connect_state_set(move |sw, on| {
            page.toggle(sw, &entry, on);
            glib::Propagation::Stop
        });
        row.add_suffix(&sw);
        row.add_suffix(&self.menu_button(&row, e));
        self.rows.borrow_mut().push((row.clone(), e.clone()));
        row
    }

    fn menu_button(self: &Rc<Self>, row: &adw::ActionRow, e: &Rc<Entry>) -> gtk::MenuButton {
        let state = e.state();
        let running = matches!(state, State::Running | State::Waiting);
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, enabled: bool, f: Rc<dyn Fn()>| {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(enabled);
            action.connect_activate(move |_, _| f());
            group.add_action(&action);
        };
        for verb in ["start", "stop", "restart"] {
            let enabled = match verb {
                "start" => !running && state != State::Skipped,
                _ => running,
            };
            let (page, entry) = (self.clone(), e.clone());
            add(verb, enabled, Rc::new(move || page.control(&entry, verb)));
        }
        let (page, entry) = (self.clone(), e.clone());
        add(
            "log",
            e.unit().is_some(),
            Rc::new(move || page.show_log(&entry)),
        );
        let (page, path) = (self.clone(), e.path().to_path_buf());
        add("folder", true, Rc::new(move || page.open_folder(&path)));
        let (page, entry) = (self.clone(), e.clone());
        add(
            "delete",
            e.deletable(),
            Rc::new(move || page.delete(&entry)),
        );
        row.insert_action_group("entry", Some(&group));

        let menu = gio::Menu::new();
        let session = gio::Menu::new();
        session.append(Some("Start"), Some("entry.start"));
        session.append(Some("Stop"), Some("entry.stop"));
        session.append(Some("Restart"), Some("entry.restart"));
        menu.append_section(None, &session);
        let info = gio::Menu::new();
        if e.unit().is_some() {
            info.append(Some("Show log"), Some("entry.log"));
        }
        info.append(Some("Open containing folder"), Some("entry.folder"));
        menu.append_section(None, &info);
        if e.deletable() {
            let del = gio::Menu::new();
            del.append(Some("Delete…"), Some("entry.delete"));
            menu.append_section(None, &del);
        }
        gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Control this session")
            .build()
    }

    fn toggle(self: &Rc<Self>, sw: &gtk::Switch, e: &Rc<Entry>, on: bool) {
        let (page, sw, e) = (self.clone(), sw.clone(), e.clone());
        glib::spawn_future_local(async move {
            let name = e.name();
            if !on && let Some(warning) = e.essential() {
                let location = match &*e {
                    Entry::Compositor(h) => h.location(),
                    _ => tilde(e.path()),
                };
                let body = format!(
                    "{warning}\n\nThis comments out {location} in your Hyprland config. You can re-enable it here."
                );
                if !page
                    .ctx
                    .confirm(&format!("Disable {name}?"), &body, "Disable", true)
                    .await
                {
                    sw.set_active(true);
                    return;
                }
            }
            sw.set_sensitive(false);
            let entry = (*e).clone();
            match rt::blocking(move || entry.set_enabled(on)).await {
                Ok(Toggled::Done) => page.ctx.toast(if on {
                    format!("{name} will start at next login")
                } else {
                    format!("{name} won't start at next login")
                }),
                Ok(Toggled::NeedsMask) => page.offer_mask(&name).await,
                Err(err) => page.ctx.error(&format!("Couldn't change {name}"), &err),
            }
            page.refresh();
        });
    }

    async fn offer_mask(&self, unit: &str) {
        let body = format!(
            "{unit} is enabled for all users in /etc/systemd/user, so disabling it for you has no effect. Masking prevents it from starting for your user at all (also on demand). Undo by switching it back on."
        );
        if !self
            .ctx
            .confirm(&format!("Mask {unit}?"), &body, "Mask", true)
            .await
        {
            return;
        }
        let name = unit.to_owned();
        match rt::blocking(move || units::mask(&name)).await {
            Ok(()) => self
                .ctx
                .toast(format!("{unit} masked; it won't start at next login")),
            Err(err) => self.ctx.error(&format!("Couldn't mask {unit}"), &err),
        }
    }

    fn control(self: &Rc<Self>, e: &Rc<Entry>, verb: &'static str) {
        let (page, e) = (self.clone(), e.clone());
        glib::spawn_future_local(async move {
            let name = e.name();
            if verb != "start"
                && let Some(warning) = e.essential()
            {
                let body = format!("{warning}\n\nThis affects the running session right now.");
                if !page
                    .ctx
                    .confirm(
                        &format!("{} {name}?", capitalize(verb)),
                        &body,
                        &capitalize(verb),
                        true,
                    )
                    .await
                {
                    return;
                }
            }
            page.spinner.set_visible(true);
            let entry = (*e).clone();
            match rt::blocking(move || entry.control(verb)).await {
                Ok(()) => page.ctx.toast(match verb {
                    "start" => format!("Started {name}"),
                    "stop" => format!("Stopped {name}"),
                    _ => format!("Restarted {name}"),
                }),
                Err(err) => page.ctx.error(&format!("Couldn't {verb} {name}"), &err),
            }
            page.refresh();
        });
    }

    fn show_log(self: &Rc<Self>, e: &Rc<Entry>) {
        let Some(unit) = e.unit().map(str::to_owned) else {
            return;
        };
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(12)
            .bottom_margin(12)
            .left_margin(12)
            .right_margin(12)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .build();
        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload log")
            .build();
        let header = adw::HeaderBar::new();
        header.pack_start(&reload);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&scroller));
        let dialog = adw::Dialog::builder()
            .title(format!("Log — {}", e.name()))
            .content_width(900)
            .content_height(600)
            .child(&tv)
            .build();

        let load = Rc::new(move || {
            let (unit, view, scroller) = (unit.clone(), view.clone(), scroller.clone());
            glib::spawn_future_local(async move {
                let u = unit.clone();
                let text = match rt::blocking(move || units::log(&u)).await {
                    Ok(t) if t.trim().is_empty() || t.trim() == "-- No entries --" => {
                        format!("No journal entries for {unit}.")
                    }
                    Ok(t) => t,
                    Err(err) => format!("Couldn't read the journal: {err:#}"),
                };
                view.buffer().set_text(&text);
                glib::idle_add_local_once(move || {
                    let adj = scroller.vadjustment();
                    adj.set_value(adj.upper());
                });
            });
        });
        let l = load.clone();
        reload.connect_clicked(move |_| l());
        load();
        dialog.present(Some(&self.ctx.window));
    }

    fn open_folder(self: &Rc<Self>, path: &Path) {
        let ctx = self.ctx.clone();
        gtk::FileLauncher::new(Some(&gio::File::for_path(path))).open_containing_folder(
            Some(&self.ctx.window),
            gio::Cancellable::NONE,
            move |r| {
                if let Err(e) = r
                    && !e.matches(gtk::DialogError::Dismissed)
                {
                    ctx.error("Couldn't open folder", &anyhow!(e));
                }
            },
        );
    }

    fn delete(self: &Rc<Self>, e: &Rc<Entry>) {
        let (page, e) = (self.clone(), e.clone());
        glib::spawn_future_local(async move {
            let entry = (*e).clone();
            let plan = rt::blocking(move || match &entry {
                Entry::App(a, props) => {
                    cleanup::for_autostart(a, matches!(props.active(), "active" | "activating"))
                }
                Entry::Service(u) => cleanup::for_unit(u),
                Entry::Compositor(_) => Err(anyhow!("compositor commands can't be deleted")),
            })
            .await;
            let plan = match plan {
                Ok(p) => p,
                Err(err) => return page.ctx.error("Can't delete", &err),
            };
            let (dialog, checks) = delete_dialog(&plan);
            if dialog.choose_future(Some(&page.ctx.window)).await != "delete" {
                return;
            }
            let chosen: Vec<bool> = checks.iter().map(|c| c.is_active()).collect();
            page.spinner.set_visible(true);
            let name = plan.name.clone();
            let report = rt::blocking(move || cleanup::execute(&plan, &chosen)).await;
            if report.errors.is_empty() {
                page.ctx.toast(format!(
                    "Deleted {name} ({} item{} removed)",
                    report.removed.len(),
                    if report.removed.len() == 1 { "" } else { "s" }
                ));
            } else {
                page.ctx.error(
                    &format!("Deleting {name} finished with problems"),
                    &anyhow!(report.errors.join("; ")),
                );
            }
            page.refresh();
        });
    }

    fn add_dialog(self: &Rc<Self>) {
        let stack = adw::ViewStack::new();
        let dialog = adw::Dialog::builder()
            .title("Add startup app")
            .content_width(560)
            .content_height(640)
            .build();

        // Installed applications.
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search applications")
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        let loading = adw::Spinner::builder().height_request(32).build();
        let s = search.clone();
        list.set_filter_func(move |row| {
            let q = s.text().to_lowercase();
            row.downcast_ref::<adw::ActionRow>().is_none_or(|r| {
                q.is_empty()
                    || r.title().to_lowercase().contains(&q)
                    || r.subtitle().is_some_and(|t| t.to_lowercase().contains(&q))
            })
        });
        let l = list.clone();
        search.connect_search_changed(move |_| l.invalidate_filter());
        let apps_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        apps_box.append(&search);
        apps_box.append(&loading);
        apps_box.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .vexpand(true)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .build(),
        );
        stack.add_titled_with_icon(
            &apps_box,
            Some("app"),
            "Application",
            "view-app-grid-symbolic",
        );

        // Custom command.
        let name = adw::EntryRow::builder().title("Name").build();
        let command = adw::EntryRow::builder().title("Command").build();
        let fields = adw::PreferencesGroup::builder()
            .description(
                "Creates a desktop entry in ~/.config/autostart that runs this command at login.",
            )
            .build();
        fields.add(&name);
        fields.add(&command);
        let add_btn = gtk::Button::builder()
            .label("Add")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let cmd_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        cmd_box.append(&fields);
        cmd_box.append(&add_btn);
        stack.add_titled_with_icon(
            &cmd_box,
            Some("command"),
            "Custom command",
            "utilities-terminal-symbolic",
        );

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&header);
        tv.set_content(Some(&stack));
        dialog.set_child(Some(&tv));

        let (page, d) = (self.clone(), dialog.clone());
        add_btn.connect_clicked(move |_| {
            let (n, c) = (name.text().to_string(), command.text().to_string());
            let program = desktop::exec_args(c.trim()).into_iter().next();
            match program {
                None => page.ctx.toast("Enter a command"),
                Some(p) if xdg::find_executable(&p).is_none() => {
                    page.ctx.toast(format!("Program not found: {p}"))
                }
                Some(_) if n.trim().is_empty() => page.ctx.toast("Enter a name"),
                Some(_) => {
                    page.finish_add(&d, n.trim().to_owned(), move || xdg::add_custom(&n, &c))
                }
            }
        });

        let (page, d) = (self.clone(), dialog.clone());
        glib::spawn_future_local(async move {
            let apps = rt::blocking(installed_apps).await;
            loading.set_visible(false);
            for app in apps {
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .subtitle_lines(1)
                    .activatable(!app.present)
                    .build();
                row.set_title(&app.name);
                row.set_subtitle(app.description.as_deref().unwrap_or_default());
                let icon = app
                    .icon
                    .as_deref()
                    .and_then(|i| gio::Icon::for_string(i).ok());
                let img = match icon {
                    Some(i) => gtk::Image::from_gicon(&i),
                    None => gtk::Image::from_icon_name("application-x-executable-symbolic"),
                };
                img.set_pixel_size(32);
                row.add_prefix(&img);
                if app.present {
                    row.add_suffix(
                        &gtk::Label::builder()
                            .label("Already added")
                            .css_classes(["dim-label"])
                            .build(),
                    );
                    row.set_sensitive(false);
                }
                let (page, d, path, name) = (page.clone(), d.clone(), app.path, app.name);
                row.connect_activated(move |_| {
                    let path = path.clone();
                    page.finish_add(&d, name.clone(), move || xdg::add_app(&path));
                });
                list.append(&row);
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn finish_add(
        self: &Rc<Self>,
        dialog: &adw::Dialog,
        name: String,
        job: impl FnOnce() -> Result<PathBuf> + Send + 'static,
    ) {
        let (page, dialog) = (self.clone(), dialog.clone());
        glib::spawn_future_local(async move {
            // daemon-reload lets the autostart generator create the unit, so Start works now.
            let res = rt::blocking(move || {
                let path = job()?;
                units::daemon_reload()?;
                Ok::<_, anyhow::Error>(path)
            })
            .await;
            match res {
                Ok(path) => {
                    dialog.close();
                    let toast = ui::plain_toast(&format!("{name} will start at next login"));
                    toast.set_button_label(Some("Start now"));
                    toast.set_timeout(6);
                    let (p, n) = (page.clone(), name.clone());
                    toast.connect_button_clicked(move |_| {
                        let id = path
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let (p, n) = (p.clone(), n.clone());
                        glib::spawn_future_local(async move {
                            match rt::blocking(move || units::control("start", &xdg::unit_for(&id)))
                                .await
                            {
                                Ok(()) => p.ctx.toast(format!("Started {n}")),
                                Err(err) => p.ctx.error(&format!("Couldn't start {n}"), &err),
                            }
                            p.refresh();
                        });
                    });
                    page.ctx.toasts.add_toast(toast);
                }
                Err(err) => page.ctx.error(&format!("Couldn't add {name}"), &err),
            }
            page.refresh();
        });
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

struct AppData {
    name: String,
    description: Option<String>,
    icon: Option<String>,
    path: PathBuf,
    /// Already has an autostart entry with this file name.
    present: bool,
}

/// Desktop file for a desktop-file id (`org.foo.App.desktop`; `-` may stand
/// for a subdirectory, e.g. `kde-foo.desktop` → `kde/foo.desktop`).
fn desktop_file_for(id: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().find_map(|d| {
        let direct = d.join(id);
        if direct.is_file() {
            return Some(direct);
        }
        id.match_indices('-')
            .map(|(i, _)| d.join(&id[..i]).join(&id[i + 1..]))
            .find(|p| p.is_file())
    })
}

fn installed_apps() -> Vec<AppData> {
    let existing: Vec<String> = xdg::scan().into_iter().map(|a| a.id).collect();
    let mut dirs = vec![glib::user_data_dir().join("applications")];
    dirs.extend(
        glib::system_data_dirs()
            .into_iter()
            .map(|d| d.join("applications")),
    );
    let mut out: Vec<AppData> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let id = a.id()?;
            let path = desktop_file_for(&id, &dirs)?;
            Some(AppData {
                name: a.display_name().to_string(),
                description: a.description().map(|d| d.to_string()),
                icon: a
                    .icon()
                    .and_then(|i| IconExt::to_string(&i))
                    .map(|s| s.to_string()),
                present: existing.iter().any(|e| *e == id.as_str()),
                path,
            })
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(6)
        .build()
}

fn item_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build()
}

/// Confirmation listing exactly what will happen; returns the dialog and the
/// opt-in checkboxes (parallel to `plan.related`).
fn delete_dialog(plan: &cleanup::Plan) -> (adw::AlertDialog, Vec<gtk::CheckButton>) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Delete {}?", plan.name)),
        Some("This can't be undone."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_prefer_wide_layout(true);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.append(&section_label("Will be done"));
    for step in plan.steps() {
        body.append(&item_label(&format!("• {step}")));
    }
    let mut checks = Vec::new();
    if !plan.related.is_empty() {
        body.append(&section_label("Also remove (optional)"));
        for r in &plan.related {
            let label = item_label(&format!("{} — {}", tilde(&r.path), r.why));
            let check = gtk::CheckButton::builder()
                .child(&label)
                .active(false)
                .build();
            body.append(&check);
            checks.push(check);
        }
    }
    if !plan.root_only.is_empty() {
        body.append(&section_label("Not removed (requires root)"));
        for item in &plan.root_only {
            body.append(&item_label(&format!("• {item}")));
        }
        if let Some(cleanup) = &plan.root_cleanup {
            let hint = item_label(&format!(
                "Its helper can remove these. Run it before deleting the helper:\n{cleanup}"
            ));
            hint.add_css_class("dim-label");
            hint.set_selectable(true);
            body.append(&hint);
            let run = gtk::Button::builder()
                .label("Run root cleanup in a terminal…")
                .halign(gtk::Align::Start)
                .css_classes(["pill"])
                .build();
            let cmdline = cleanup.clone();
            run.connect_clicked(move |b| {
                if let Err(e) = hyprdeck_core::cmd::spawn_in_terminal("Root cleanup", &cmdline) {
                    b.set_label(&format!("Couldn't open terminal: {e:#}"));
                }
            });
            body.append(&run);
        }
    }
    if !plan.left.is_empty() {
        body.append(&section_label("Left in place"));
        for item in &plan.left {
            body.append(&item_label(&format!("• {item}")));
        }
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(420)
        .build();
    dialog.set_extra_child(Some(&scroller));
    (dialog, checks)
}
