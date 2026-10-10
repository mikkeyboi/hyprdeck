//! Native GTK renderer. Backend data is plain text, never markup or commands.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::{
    rt,
    ui::{self, Ctx},
};
use serde_json::{Map, Value};

use crate::backend::{self, Installed};
use crate::controller::ControllerView;
use crate::products::Products;
use crate::protocol::{Control, State};
use crate::releases;

type PageRef = Rc<Page>;
struct Page {
    ctx: Ctx,
    root: glib::WeakRef<gtk::ScrolledWindow>,
    inventory: gtk::Box,
    navigation: gtk::Stack,
    integrations: gtk::Box,
    status: gtk::Label,
    source: adw::EntryRow,
    install: gtk::Button,
    reload: gtk::Button,
    refresh_interval_ms: Cell<u64>,
    busy: Cell<bool>,
    loop_started: Cell<bool>,
    plugins: RefCell<Vec<Rc<PluginView>>>,
}
struct PluginView {
    id: String,
    enabled: bool,
    state: gtk::Box,
    diagnostics: gtk::Label,
    release: gtk::Label,
    update: gtk::Button,
    dirty: Cell<bool>,
    rendered: RefCell<Option<Rendered>>,
    products: RefCell<Option<Products>>,
}
struct Rendered {
    state: State,
    title: gtk::Label,
    description: gtk::Label,
    groups: Vec<(adw::PreferencesGroup, Vec<adw::ActionRow>)>,
    controllers: Vec<Option<ControllerView>>,
}

enum FormInput {
    Number(adw::SpinRow),
    Text(adw::EntryRow),
    Color(gtk::ColorDialogButton),
    Choice(adw::ComboRow, Vec<crate::protocol::OptionItem>),
    Switch(adw::SwitchRow),
}
impl FormInput {
    fn value(&self) -> Value {
        match self {
            Self::Number(row) => {
                row.update();
                number_value(row.value())
            }
            Self::Text(row) => Value::String(row.text().to_string()),
            Self::Color(widget) => Value::String(color_hex(&widget.rgba())),
            Self::Choice(row, options) => {
                Value::String(options[row.selected() as usize].value.clone())
            }
            Self::Switch(row) => Value::Bool(row.is_active()),
        }
    }
}
fn number_value(value: f64) -> Value {
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value < -(i64::MIN as f64) {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}
fn color_hex(color: &gtk::gdk::RGBA) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        (color.red() * 255.0).round() as u8,
        (color.green() * 255.0).round() as u8,
        (color.blue() * 255.0).round() as u8
    )
}
fn color_picker(value: &str) -> gtk::ColorDialogButton {
    let dialog = gtk::ColorDialog::builder()
        .title("Choose lighting color")
        .with_alpha(false)
        .build();
    let widget = gtk::ColorDialogButton::new(Some(dialog));
    widget.set_rgba(&gtk::gdk::RGBA::parse(value).expect("validated RGB color"));
    widget.set_valign(gtk::Align::Center);
    widget
}
fn mark_edit(view: &Rc<PluginView>, row: &adw::ActionRow) {
    view.dirty.set(true);
    row.add_css_class("hd-pending-edit");
}

fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .build()
}
fn group(title: &str, description: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(title))
        .description(glib::markup_escape_text(description))
        .build()
}
fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}
fn button(text: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(text)
        .valign(gtk::Align::Center)
        .build()
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (root, content) = ui::page_scaffold();
    let management = group("External plugins", backend::TRUST);
    let source = adw::EntryRow::builder()
        .use_markup(false)
        .show_apply_button(false)
        .build();
    source.set_title("Local folder or GitHub owner/repo");
    let install = button("Install");
    source.add_suffix(&install);
    management.add(&source);
    let reload = button("Reload installed plugins");
    let row = action_row(
        "Installed plugins",
        "Install first, then explicitly enable code you trust. Updates never enable disabled plugins.",
    );
    row.add_suffix(&reload);
    management.add(&row);
    let administration = gtk::Expander::builder()
        .label("Manage plugins")
        .child(&management)
        .build();
    let status = label("");
    content.append(&status);
    status.set_visible(false);
    let integrations = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let installed_heading = label("Installed integrations");
    installed_heading.add_css_class("title-3");
    let navigation = gtk::Stack::builder()
        .hhomogeneous(false)
        .vhomogeneous(false)
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .build();
    let home = gtk::Box::new(gtk::Orientation::Vertical, 18);
    home.append(&administration);
    home.append(&installed_heading);
    home.append(&integrations);
    let products_heading = label("Your products");
    products_heading.add_css_class("title-1");
    home.append(&products_heading);
    let inventory = gtk::Box::new(gtk::Orientation::Vertical, 24);
    home.append(&inventory);
    navigation.add_named(&home, Some("products"));
    content.append(&navigation);
    let page = Rc::new(Page {
        ctx: ctx.clone(),
        root: root.downgrade(),
        inventory,
        navigation,
        integrations,
        status,
        source,
        install,
        reload,
        busy: Cell::new(false),
        loop_started: Cell::new(false),
        refresh_interval_ms: Cell::new(2000),
        plugins: RefCell::new(Vec::new()),
    });
    let scroller = root.downgrade();
    page.navigation.connect_visible_child_notify(move |_| {
        if let Some(root) = scroller.upgrade() {
            root.vadjustment().set_value(0.0);
        }
    });
    let weak = Rc::downgrade(&page);
    page.install.connect_clicked(move |_| {
        if let Some(page) = weak.upgrade() {
            let source = page.source.text().trim().to_owned();
            if source.is_empty() {
                page.ctx.toast("Enter a local folder or GitHub owner/repo.");
                return;
            }
            if !page.begin("Installing without executing…") {
                return;
            }
            let ctx = page.ctx.clone();
            ctx.spawn(async move {
                let result = rt::run(async move {
                    let path = std::path::PathBuf::from(&source);
                    if path.is_dir() || source.starts_with('/') || source.starts_with('.') {
                        backend::install_local(path).await
                    } else {
                        releases::install(&source).await
                    }
                })
                .await;
                match result {
                    Ok(manifest) => {
                        page.source.set_text("");
                        page.ctx.toast(format!(
                            "Installed {} {}. It is disabled; enable only after reviewing trust.",
                            manifest.name, manifest.version
                        ));
                        page.load().await;
                    }
                    Err(error) => page.fail("Install failed", &error),
                }
                page.end();
            });
        }
    });
    let weak = Rc::downgrade(&page);
    page.reload.connect_clicked(move |_| {
        if let Some(page) = weak.upgrade() {
            if !page.begin("Loading installed plugins…") {
                return;
            }
            let ctx = page.ctx.clone();
            ctx.spawn(async move {
                page.load().await;
                page.end();
            });
        }
    });
    let page_on_map = page.clone();
    root.connect_map(move |_| {
        if page_on_map.loop_started.replace(true) {
            return;
        }
        let page = page_on_map.clone();
        let ctx = page.ctx.clone();
        ctx.spawn(async move {
            loop {
                let Some(root) = page.root.upgrade() else {
                    break;
                };
                if root.is_mapped() && !page.busy.get() {
                    page.begin("Refreshing…");
                    if page.plugins.borrow().is_empty() {
                        page.load().await;
                    }
                    page.refresh().await;
                    page.end();
                }
                drop(root);
                glib::timeout_future(Duration::from_millis(page.refresh_interval_ms.get())).await;
            }
        });
    });
    root.upcast()
}

impl Page {
    fn begin(&self, message: &str) -> bool {
        if self.busy.replace(true) {
            self.ctx
                .toast("A plugin operation is in progress. Please wait.");
            return false;
        }
        self.install.set_sensitive(false);
        self.reload.set_sensitive(false);
        self.status.set_text(message);
        self.status.set_visible(message != "Refreshing…");
        true
    }
    fn end(&self) {
        self.busy.set(false);
        self.install.set_sensitive(true);
        self.reload.set_sensitive(true);
        if matches!(
            self.status.text().as_str(),
            "Refreshing…"
                | "Loading installed plugins…"
                | "Installing without executing…"
                | "Running plugin action…"
                | "Checking release…"
                | "Applying verified update…"
                | "Changing activation…"
        ) {
            self.status.set_text("");
            self.status.set_visible(false);
        }
    }
    fn fail(&self, what: &str, error: &anyhow::Error) {
        self.status.set_text(&format!("{what}: {error:#}"));
        self.status.set_visible(true);
        self.ctx.error(what, error);
    }
    async fn load(self: &PageRef) {
        let result = rt::blocking(backend::list).await;
        match result {
            Ok(plugins) => {
                while let Some(child) = self.inventory.first_child() {
                    self.inventory.remove(&child);
                }
                while let Some(child) = self.integrations.first_child() {
                    self.integrations.remove(&child);
                }
                self.navigation.set_visible_child_name("products");
                let pages: Vec<_> = self
                    .navigation
                    .pages()
                    .iter::<gtk::StackPage>()
                    .filter_map(Result::ok)
                    .filter(|page| page.name().as_deref() != Some("products"))
                    .map(|page| page.child())
                    .collect();
                for child in pages {
                    self.navigation.remove(&child);
                }
                self.plugins.borrow_mut().clear();
                if plugins.is_empty() {
                    self.inventory.append(&label("No plugins installed. Install a local folder or a repository with a stable binary release."));
                }
                for installed in plugins {
                    self.add_plugin(installed);
                }
            }
            Err(error) => self.fail("Cannot read plugin configuration", &error),
        }
    }
    fn add_plugin(self: &PageRef, installed: Installed) {
        let name = installed
            .manifest
            .as_ref()
            .map(|m| m.name.as_str())
            .unwrap_or(&installed.id);
        let description = installed
            .manifest
            .as_ref()
            .map(|m| {
                format!(
                    "{} · {} · {}\n{}",
                    m.id, m.version, m.update_repo, m.description
                )
            })
            .unwrap_or_else(|| "Invalid or missing installation; execution is blocked.".into());
        let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let management = group(name, &description);
        let row = action_row(
            if installed.enabled {
                "Enabled / trusted"
            } else {
                "Disabled / not executing"
            },
            "Enable runs an API handshake. Disable stops future state and action requests.",
        );
        let toggle = button(if installed.enabled {
            "Disable"
        } else {
            "Enable and trust…"
        });
        toggle.set_sensitive(installed.enabled || installed.manifest.is_some());
        row.add_suffix(&toggle);
        management.add(&row);
        let check = button("Check release");
        let update = button("Apply update…");
        update.set_sensitive(false);
        check.set_sensitive(installed.manifest.is_some());
        let updates = action_row(
            "Independent binary releases",
            "SHA256 verified; incompatible, mismatched or older releases are rejected.",
        );
        updates.add_suffix(&check);
        updates.add_suffix(&update);
        management.add(&updates);
        let settings = gtk::Expander::builder()
            .label(format!(
                "{name} · {}",
                if installed.enabled {
                    "Enabled"
                } else {
                    "Disabled"
                }
            ))
            .child(&management)
            .build();
        self.integrations.append(&settings);
        let diagnostics = label(installed.error.as_deref().unwrap_or(if installed.enabled {
            "Waiting for state…"
        } else {
            "Disabled: no backend has been executed."
        }));
        management.add(&diagnostics);
        diagnostics.add_css_class("dim-label");
        let release = label("");
        management.add(&release);
        let state = gtk::Box::new(gtk::Orientation::Vertical, 12);
        container.prepend(&state);
        self.inventory.append(&container);
        let view = Rc::new(PluginView {
            id: installed.id.clone(),
            enabled: installed.enabled,
            state,
            diagnostics,
            release,
            update: update.clone(),
            dirty: Cell::new(false),
            rendered: RefCell::new(None),
            products: RefCell::new(None),
        });
        self.plugins.borrow_mut().push(view.clone());
        let weak = Rc::downgrade(self);
        let id = installed.id;
        let enabled = installed.enabled;
        toggle.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                if !page.begin("Changing activation…") {
                    return;
                }
                let id = id.clone();
                let ctx = page.ctx.clone();
                ctx.spawn(async move {
                    if !enabled
                        && !page
                            .ctx
                            .confirm(
                                "Trust and enable plugin?",
                                backend::TRUST,
                                "Trust and enable",
                                false,
                            )
                            .await
                    {
                        page.end();
                        return;
                    }
                    let result = rt::run(async move {
                        if enabled {
                            backend::disable(&id).await
                        } else {
                            backend::enable(&id).await
                        }
                    })
                    .await;
                    match result {
                        Ok(()) => page.load().await,
                        Err(error) => page.fail("Activation failed", &error),
                    }
                    page.end();
                });
            }
        });
        let weak = Rc::downgrade(self);
        let target = Rc::downgrade(&view);
        check.connect_clicked(move |_| {
            if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                if !page.begin("Checking release…") {
                    return;
                }
                let id = view.id.clone();
                let ctx = page.ctx.clone();
                ctx.spawn(async move {
                    match rt::run(async move { releases::check(&id, true).await }).await {
                        Ok(check) => {
                            view.release.set_text(&format!(
                                "{} → {} · {}\n{}",
                                check.installed_version,
                                check.available_version,
                                if check.update_available {
                                    "Update available"
                                } else {
                                    "Up to date"
                                },
                                check.url
                            ));
                            view.update.set_sensitive(check.update_available);
                        }
                        Err(error) => {
                            view.release
                                .set_text(&format!("Release check failed: {error:#}"));
                            view.update.set_sensitive(false);
                            page.ctx.error("Release check failed", &error);
                        }
                    }
                    page.end();
                });
            }
        });
        let weak = Rc::downgrade(self);
        let target = Rc::downgrade(&view);
        update.connect_clicked(move |_| if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
            if !page.begin("Applying verified update…") { return; }
            let ctx = page.ctx.clone(); ctx.spawn(async move {
                let body = if view.enabled { "Download and SHA256-verify the latest compatible stable release, then run its state handshake. The plugin stays enabled. A failed handshake restores the previous version. Only continue if you trust this release publisher." } else { "Download and SHA256-verify the latest compatible stable release. The plugin stays disabled and the new executable is not run until you explicitly enable it." };
                if !page.ctx.confirm("Apply plugin update?", body, "Apply update", false).await { page.end(); return; }
                let id = view.id.clone();
                match rt::run(async move { releases::update(&id).await }).await {
                    Ok(manifest) => { page.ctx.toast(format!("Updated {} to {}; activation preserved.", manifest.id, manifest.version)); page.load().await; }
                    Err(error) => { view.release.set_text(&format!("Update failed: {error:#}")); page.fail("Update failed", &error); }
                }
                page.end();
            });
        });
    }
    async fn refresh(self: &PageRef) {
        let plugins = self.plugins.borrow().clone();
        self.refresh_interval_ms.set(2000);
        for view in plugins {
            if !view.enabled {
                continue;
            }
            if !self.root.upgrade().is_some_and(|root| root.is_mapped()) {
                break;
            }
            let id = view.id.clone();
            match rt::run(async move { backend::request(&id, None, Map::new()).await }).await {
                Ok(state) => {
                    view.diagnostics.set_text(if view.dirty.get() { "Live state received. Pending edits are preserved; Apply to send, or Reload to discard." } else { "Backend connected. State refreshes while this page is visible." });
                    view.state.set_sensitive(true);
                    self.render(&view, state);
                }
                Err(error) => {
                    view.diagnostics
                        .set_text(&format!("Backend error (will retry): {error:#}"));
                    view.state.set_sensitive(false);
                    if let Some(products) = view.products.borrow().as_ref() {
                        products.disconnect();
                    }
                }
            }
        }
    }
    fn run_action(
        self: &PageRef,
        view: &Rc<PluginView>,
        action: String,
        args: Map<String, Value>,
        destructive: bool,
        submitted: Option<adw::ActionRow>,
    ) {
        if !self.begin("Running plugin action…") {
            return;
        }
        let page = self.clone();
        let view = view.clone();
        let ctx = self.ctx.clone();
        ctx.spawn(async move {
            if destructive && !page.ctx.confirm("Run destructive plugin action?", "This action is marked destructive by the plugin. It runs unsandboxed as your user.", "Run action", true).await { page.end(); return; }
            view.state.set_sensitive(false);
            if let Some(products) = view.products.borrow().as_ref() { products.set_sensitive(false); }
            let id = view.id.clone();
            let result = rt::run(async move { backend::request(&id, Some(&action), args).await }).await;
            match result {
                Ok(state) => {
                    if let Some(row) = submitted { row.remove_css_class("hd-pending-edit"); }
                    view.dirty.set(false);
                    view.diagnostics.set_text("Action completed; refreshed state received.");
                    page.render(&view, state);
                }
                Err(error) => { view.diagnostics.set_text(&format!("Action failed: {error:#}")); page.ctx.error("Plugin action failed", &error); }
            }
            view.state.set_sensitive(true); page.end();
            if let Some(products) = view.products.borrow().as_ref() { products.set_sensitive(true); }
        });
    }
    fn render(self: &PageRef, view: &Rc<PluginView>, state: State) {
        self.refresh_interval_ms.set(
            self.refresh_interval_ms
                .get()
                .min(state.refresh_interval_ms),
        );
        if !state.products.is_empty() || view.products.borrow().is_some() {
            if view.products.borrow().is_none() {
                while let Some(child) = view.state.first_child() {
                    view.state.remove(&child);
                }
                view.rendered.borrow_mut().take();
                let page = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let products = Products::new(&view.id, &self.navigation, move |row, control| {
                    if let (Some(page), Some(view)) = (page.upgrade(), target.upgrade()) {
                        page.control(&view, row, control);
                    }
                });
                view.state.append(&products.widget);
                *view.products.borrow_mut() = Some(products);
            }
            let focus = gtk::prelude::GtkWindowExt::focus(&self.ctx.window);
            if let Some(products) = view.products.borrow().as_ref() {
                products.update(&state, false, focus.as_ref());
            }
            return;
        }
        let focused_edit =
            gtk::prelude::GtkWindowExt::focus(&self.ctx.window).is_some_and(|focus| {
                focus.is_ancestor(&view.state)
                    && (focus.is::<gtk::Entry>()
                        || focus.is::<gtk::Text>()
                        || focus.is::<gtk::SpinButton>()
                        || focus.is::<gtk::DropDown>()
                        || focus.ancestor(gtk::SpinButton::static_type()).is_some()
                        || focus.ancestor(gtk::DropDown::static_type()).is_some())
            });
        let preserve_controls = view.dirty.get() || focused_edit;
        {
            let rendered = view.rendered.borrow();
            if let Some(old) = rendered.as_ref() {
                if old.state == state {
                    return;
                }
                let same_shape = old.state.groups.len() == state.groups.len()
                    && old.state.groups.iter().zip(&state.groups).all(|(a, b)| {
                        a.id == b.id
                            && a.collapsed == b.collapsed
                            && a.visualization.is_some() == b.visualization.is_some()
                            && a.rows.len() == b.rows.len()
                            && a.rows.iter().zip(&b.rows).all(|(a, b)| a.id == b.id)
                    });
                if same_shape {
                    old.title.set_text(&state.title);
                    for (controller, section) in old.controllers.iter().zip(&state.groups) {
                        if let (Some(controller), Some(visualization)) =
                            (controller, &section.visualization)
                        {
                            controller.update(visualization);
                        }
                    }
                    old.description.set_text(&state.description);
                    for ((widgets, old_group), new_group) in
                        old.groups.iter().zip(&old.state.groups).zip(&state.groups)
                    {
                        if old_group.title != new_group.title {
                            widgets
                                .0
                                .set_title(&glib::markup_escape_text(&new_group.title));
                        }
                        if old_group.description != new_group.description {
                            widgets.0.set_description(Some(&glib::markup_escape_text(
                                &new_group.description,
                            )));
                        }
                        for ((row, old_row), new_row) in
                            widgets.1.iter().zip(&old_group.rows).zip(&new_group.rows)
                        {
                            if old_row.title != new_row.title {
                                row.set_title(&new_row.title);
                            }
                            if old_row.subtitle != new_row.subtitle {
                                row.set_subtitle(&new_row.subtitle);
                            }
                        }
                    }
                    let controls_equal =
                        old.state.groups.iter().zip(&state.groups).all(|(a, b)| {
                            a.rows
                                .iter()
                                .zip(&b.rows)
                                .all(|(a, b)| a.control == b.control)
                        });
                    if preserve_controls || controls_equal {
                        drop(rendered);
                        if !preserve_controls
                            && let Some(rendered) = view.rendered.borrow_mut().as_mut()
                        {
                            rendered.state = state;
                        }
                        return;
                    }
                } else if preserve_controls {
                    return;
                }
            }
        }
        while let Some(child) = view.state.first_child() {
            view.state.remove(&child);
        }
        view.state.set_sensitive(true);
        let title = label(&state.title);
        let description = label(&state.description);
        let has_visualization = state
            .groups
            .iter()
            .any(|section| section.visualization.is_some());
        title.set_visible(!has_visualization);
        description.set_visible(!has_visualization);
        view.state.append(&title);
        view.state.append(&description);
        let mut groups = Vec::new();
        let mut controllers = Vec::new();
        for section in &state.groups {
            let controller = section.visualization.as_ref().map(|visualization| {
                let page = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let controller = ControllerView::new(visualization, move |row, control| {
                    if let (Some(page), Some(view)) = (page.upgrade(), target.upgrade()) {
                        page.control(&view, row, control);
                    }
                });
                view.state.append(&controller.widget);
                controller
            });
            controllers.push(controller);
            let group = group(&section.title, &section.description);
            let mut rows = Vec::new();
            for item in &section.rows {
                let row = action_row(&item.title, &item.subtitle);
                if let Some(control) = &item.control {
                    self.control(view, &row, control.clone());
                }
                group.add(&row);
                rows.push(row);
            }
            if section.collapsed {
                let details = gtk::Expander::builder()
                    .label(&section.title)
                    .child(&group)
                    .build();
                view.state.append(&details);
            } else {
                view.state.append(&group);
            }
            groups.push((group, rows));
        }
        *view.rendered.borrow_mut() = Some(Rendered {
            state,
            title,
            description,
            groups,
            controllers,
        });
    }
    fn control(self: &PageRef, view: &Rc<PluginView>, row: &adw::ActionRow, control: Control) {
        match control {
            Control::Button {
                label,
                action,
                args,
                destructive,
            } => {
                let widget = button(&label);
                if destructive {
                    widget.add_css_class("destructive-action");
                }
                row.add_suffix(&widget);
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                widget.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        page.run_action(&view, action.clone(), args.clone(), destructive, None);
                    }
                });
            }
            Control::Switch {
                value,
                action,
                args,
            } => {
                let widget = gtk::Switch::builder()
                    .active(value)
                    .valign(gtk::Align::Center)
                    .build();
                widget.update_property(&[gtk::accessible::Property::Label(&row.title())]);
                row.add_suffix(&widget);
                let apply = button("Apply");
                row.add_suffix(&apply);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_active_notify(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        let mut args = args.clone();
                        args.insert("value".into(), Value::Bool(widget.is_active()));
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
            Control::Number {
                value,
                min,
                max,
                step,
                action,
                args,
            } => {
                let adjustment = gtk::Adjustment::new(value, min, max, step, step * 10.0, 0.0);
                let widget = gtk::SpinButton::new(
                    Some(&adjustment),
                    step,
                    if step.fract() == 0.0 { 0 } else { 6 },
                );
                widget.set_valign(gtk::Align::Center);
                widget.set_width_chars(8);
                widget.update_property(&[gtk::accessible::Property::Label(&row.title())]);
                let apply = button("Apply");
                row.add_suffix(&widget);
                row.add_suffix(&apply);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_value_changed(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_changed(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        widget.update();
                        let value = number_value(widget.value());
                        let mut args = args.clone();
                        args.insert("value".into(), value);
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
            Control::Choice {
                value,
                options,
                action,
                args,
            } => {
                let labels: Vec<_> = options.iter().map(|option| option.label.as_str()).collect();
                let widget = gtk::DropDown::from_strings(&labels);
                widget.set_valign(gtk::Align::Center);
                widget.set_selected(
                    options
                        .iter()
                        .position(|option| option.value == value)
                        .unwrap_or(0) as u32,
                );
                widget.update_property(&[gtk::accessible::Property::Label(&row.title())]);
                row.add_suffix(&widget);
                let apply = button("Apply");
                row.add_suffix(&apply);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_selected_notify(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade())
                        && let Some(option) = options.get(widget.selected() as usize)
                    {
                        let mut args = args.clone();
                        args.insert("value".into(), Value::String(option.value.clone()));
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
            Control::Text {
                value,
                action,
                args,
            } => {
                let widget = gtk::Entry::builder()
                    .text(value)
                    .valign(gtk::Align::Center)
                    .max_length(16_384)
                    .build();
                widget.update_property(&[gtk::accessible::Property::Label(&row.title())]);
                let apply = button("Apply");
                row.add_suffix(&widget);
                row.add_suffix(&apply);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_changed(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        let mut args = args.clone();
                        args.insert("value".into(), Value::String(widget.text().to_string()));
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
            Control::Color {
                value,
                action,
                args,
            } => {
                let widget = color_picker(&value);
                widget.update_property(&[gtk::accessible::Property::Label(&row.title())]);
                let apply = button("Apply");
                row.add_suffix(&widget);
                row.add_suffix(&apply);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                widget.connect_rgba_notify(move |_| {
                    if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                        mark_edit(&view, &row);
                    }
                });
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        let mut args = args.clone();
                        args.insert("value".into(), Value::String(color_hex(&widget.rgba())));
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
            Control::Form {
                fields,
                action,
                args,
                label: apply_label,
            } => {
                // A nested native list keeps related effect/color or DPI stage fields together.
                let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
                form.set_hexpand(true);
                let heading = label(&row.title());
                heading.add_css_class("heading");
                row.bind_property("title", &heading, "label")
                    .sync_create()
                    .build();
                form.append(&heading);
                let description = label(row.subtitle().as_deref().unwrap_or(""));
                row.bind_property("subtitle", &description, "label")
                    .sync_create()
                    .build();
                description
                    .set_visible(row.subtitle().is_some_and(|subtitle| !subtitle.is_empty()));
                form.append(&description);
                form.set_margin_top(12);
                form.set_margin_bottom(12);
                form.set_margin_start(12);
                form.set_margin_end(12);
                let list = gtk::ListBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .build();
                list.add_css_class("boxed-list");
                form.append(&list);
                let mut inputs = Vec::with_capacity(fields.len());
                for field in fields {
                    let target = Rc::downgrade(view);
                    let edited = row.downgrade();
                    let changed = move || {
                        if let (Some(view), Some(row)) = (target.upgrade(), edited.upgrade()) {
                            mark_edit(&view, &row);
                        }
                    };
                    let input = match field.kind.as_str() {
                        "number" => {
                            let step = field.step.expect("validated number step");
                            let adjustment = gtk::Adjustment::new(
                                field.value.as_f64().expect("validated number"),
                                field.min.expect("validated minimum"),
                                field.max.expect("validated maximum"),
                                step,
                                step * 10.0,
                                0.0,
                            );
                            let input = adw::SpinRow::builder()
                                .use_markup(false)
                                .adjustment(&adjustment)
                                .digits(if step.fract() == 0.0 { 0 } else { 6 })
                                .build();
                            input.set_title(&field.label);
                            let changed_value = changed.clone();
                            input.connect_value_notify(move |_| changed_value());
                            input.connect_changed(move |_| changed());
                            list.append(&input);
                            FormInput::Number(input)
                        }
                        "text" => {
                            let input = adw::EntryRow::builder()
                                .use_markup(false)
                                .show_apply_button(false)
                                .build();
                            input.set_title(&field.label);
                            input.set_text(field.value.as_str().expect("validated text"));
                            input.connect_changed(move |_| changed());
                            list.append(&input);
                            FormInput::Text(input)
                        }
                        "color" => {
                            let input =
                                color_picker(field.value.as_str().expect("validated color"));
                            input
                                .update_property(&[gtk::accessible::Property::Label(&field.label)]);
                            let field_row = action_row(&field.label, "");
                            field_row.add_suffix(&input);
                            field_row.set_activatable_widget(Some(&input));
                            input.connect_rgba_notify(move |_| changed());
                            list.append(&field_row);
                            FormInput::Color(input)
                        }
                        "choice" => {
                            let labels: Vec<_> = field
                                .options
                                .iter()
                                .map(|option| option.label.as_str())
                                .collect();
                            let model = gtk::StringList::new(&labels);
                            let input = adw::ComboRow::builder()
                                .use_markup(false)
                                .model(&model)
                                .build();
                            input.set_title(&field.label);
                            input.set_selected(
                                field
                                    .options
                                    .iter()
                                    .position(|option| {
                                        Some(option.value.as_str()) == field.value.as_str()
                                    })
                                    .expect("validated choice")
                                    as u32,
                            );
                            input.connect_selected_notify(move |_| changed());
                            list.append(&input);
                            FormInput::Choice(input, field.options)
                        }
                        "switch" => {
                            let input = adw::SwitchRow::builder()
                                .use_markup(false)
                                .active(field.value.as_bool().expect("validated boolean"))
                                .build();
                            input.set_title(&field.label);
                            input.connect_active_notify(move |_| changed());
                            list.append(&input);
                            FormInput::Switch(input)
                        }
                        _ => unreachable!("validated form kind"),
                    };
                    inputs.push((field.id, input));
                }
                let apply = button(&apply_label);
                apply.add_css_class("suggested-action");
                apply.set_halign(gtk::Align::End);
                form.append(&apply);
                // Keep related fields full-width even on narrow windows.
                row.set_child(Some(&form));
                let weak = Rc::downgrade(self);
                let target = Rc::downgrade(view);
                let edited = row.downgrade();
                apply.connect_clicked(move |_| {
                    if let (Some(page), Some(view)) = (weak.upgrade(), target.upgrade()) {
                        let values: Map<String, Value> = inputs
                            .iter()
                            .map(|(id, input)| (id.clone(), input.value()))
                            .collect();
                        let mut args = args.clone();
                        args.insert("value".into(), Value::Object(values));
                        page.run_action(&view, action.clone(), args, false, edited.upgrade());
                    }
                });
            }
        }
    }
}
