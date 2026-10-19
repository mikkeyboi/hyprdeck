//! Compatibility-tool releases, installed inventory and non-destructive prefix recovery.
use crate::{
    proton::{self, InstalledTool, Provider, Release, Target, TargetKind},
    proton_repair, recovery,
    steam::{self, Game},
    steam_assign,
};
use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::{
    rt, store,
    ui::{self, Ctx},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    custom: Vec<CustomTarget>,
}
#[derive(Clone, Serialize, Deserialize)]
struct CustomTarget {
    name: String,
    root: PathBuf,
    kind: String,
}
impl CustomTarget {
    fn target(&self) -> Target {
        Target {
            id: format!("custom:{}:{}", self.kind, self.root.display()),
            name: self.name.clone(),
            root: self.root.clone(),
            kind: match self.kind.as_str() {
                "Lutris" => TargetKind::Lutris,
                "Heroic" => TargetKind::Heroic,
                "Bottles" => TargetKind::Bottles,
                _ => TargetKind::Steam,
            },
            launcher_root: None,
            steam_packaging: None,
        }
    }
}
struct ToolEntry {
    tool: InstalledTool,
    capabilities: proton::Capabilities,
}
struct Inventory {
    targets: Vec<Target>,
    tools: Vec<ToolEntry>,
    steam: steam::Inventory,
    warnings: Vec<String>,
}
fn inventory() -> anyhow::Result<Inventory> {
    let mut targets = proton::targets()?;
    for custom in store::load::<Settings>("compatibility-tools")?.custom {
        let target = custom.target();
        if !targets
            .iter()
            .any(|t| t.root == target.root && t.kind == target.kind)
        {
            targets.push(target);
        }
    }
    for target in &mut targets {
        if target.kind == TargetKind::Steam
            && let Ok(root) = proton::steam_root(target)
        {
            target.launcher_root = Some(root);
        }
    }
    let mut tools = Vec::new();
    let mut warnings = Vec::new();
    for target in &targets {
        match proton::installed(target) {
            Ok(installed) => {
                for tool in installed {
                    let capabilities =
                        proton::capabilities(target, &tool).unwrap_or_else(|error| {
                            proton::Capabilities {
                                editable: false,
                                removable: false,
                                reason: format!("{error:#}"),
                            }
                        });
                    tools.push(ToolEntry { tool, capabilities });
                }
            }
            Err(e) => warnings.push(format!("{}: {e:#}", target.name)),
        }
    }
    let steam = steam::inventory(&targets)?;
    warnings.extend(steam.warnings.clone());
    Ok(Inventory {
        targets,
        tools,
        steam,
        warnings,
    })
}
struct Page {
    ctx: Ctx,
    target: adw::ComboRow,
    provider: adw::ComboRow,
    providers: Vec<Provider>,
    search: gtk::SearchEntry,
    refresh: gtk::Button,
    fetch: gtk::Button,
    status: gtk::Label,
    installed: gtk::Box,
    games: gtk::Box,
    releases: gtk::ListBox,
    notes: gtk::TextBuffer,
    release_title: gtk::Label,
    release_info: gtk::Label,
    release_link: gtk::LinkButton,
    install: gtk::Button,
    repair: gtk::CheckButton,
    cancel: gtk::Button,
    progress: gtk::ProgressBar,
    inventory: RefCell<Option<Inventory>>,
    release_data: RefCell<Vec<Release>>,
    busy: Cell<bool>,
    syncing: Cell<bool>,
    revision: Cell<u64>,
    cancel_flag: RefCell<Option<Arc<AtomicBool>>>,
    notice: RefCell<Option<String>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    let settings = adw::PreferencesGroup::new();
    let target = adw::ComboRow::builder()
        .title("Launcher / installation target")
        .subtitle("Steam tools, Lutris Wine runners, Heroic and Bottles are separate installations")
        .build();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let refresh = gtk::Button::with_label("Refresh installed");
    let custom = gtk::Button::with_label("Add target…");
    actions.append(&refresh);
    actions.append(&custom);
    settings.set_header_suffix(Some(&actions));
    settings.add(&target);
    root.append(&settings);
    let status = gtk::Label::builder()
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    root.append(&status);
    let stack = adw::ViewStack::new();
    stack.set_vexpand(true);
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .halign(gtk::Align::Center)
        .build();
    root.append(&switcher);
    let browse = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let provider = adw::ComboRow::builder()
        .title("Compatibility tool provider")
        .build();
    let fetch = gtk::Button::with_label("Load releases");
    fetch.set_valign(gtk::Align::Center);
    provider.add_suffix(&fetch);
    let provider_group = adw::PreferencesGroup::new();
    provider_group.add(&provider);
    browse.append(&provider_group);
    let releases = gtk::ListBox::new();
    releases.add_css_class("boxed-list");
    releases.set_selection_mode(gtk::SelectionMode::Single);
    let release_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_width(200)
        .vexpand(true)
        .child(&releases)
        .build();
    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    details.set_margin_start(16);
    details.set_margin_end(8);
    let release_title = gtk::Label::builder()
        .label("Select a release")
        .wrap(true)
        .xalign(0.)
        .css_classes(["title-3"])
        .build();
    details.append(&release_title);
    let release_info = gtk::Label::builder()
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    details.append(&release_info);
    let release_link = gtk::LinkButton::with_label("https://github.com", "Open upstream release");
    release_link.set_halign(gtk::Align::Start);
    release_link.set_visible(false);
    details.append(&release_link);
    let notes = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    details.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&notes)
            .build(),
    );
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    details.append(&progress);
    let repair = gtk::CheckButton::with_label("Repair recognized prefix parser (opt-in)");
    repair.set_tooltip_text(Some(proton_repair::WARNING));
    repair.set_visible(false);
    details.append(&repair);
    let install = gtk::Button::with_label("Install selected release");
    install.add_css_class("suggested-action");
    install.set_sensitive(false);
    let cancel = gtk::Button::with_label("Cancel download");
    cancel.set_visible(false);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.append(&install);
    buttons.append(&cancel);
    details.append(&buttons);
    let panes = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&release_scroll)
        .end_child(&details)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(250)
        .vexpand(true)
        .build();
    browse.append(&panes);
    stack.add_titled_with_icon(
        &browse,
        Some("releases"),
        "Download tools",
        "folder-download-symbolic",
    );
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search installed tools or games…")
        .build();
    let installed = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let games = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for (id, title, icon, body) in [
        (
            "installed",
            "Installed tools",
            "package-x-generic-symbolic",
            &installed,
        ),
        (
            "games",
            "Steam games & recovery",
            "input-gaming-symbolic",
            &games,
        ),
    ] {
        stack.add_titled_with_icon(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(body)
                .build(),
            Some(id),
            title,
            icon,
        );
    }
    root.append(&search);
    root.append(&stack);
    let providers = proton::providers();
    provider.set_model(Some(&gtk::StringList::new(
        &providers.iter().map(|p| p.name).collect::<Vec<_>>(),
    )));
    let page = Rc::new(Page {
        ctx: ctx.clone(),
        target,
        provider,
        providers,
        search,
        refresh,
        fetch,
        status,
        installed,
        games,
        releases,
        notes: notes.buffer(),
        release_title,
        release_info,
        release_link,
        install,
        repair,
        cancel,
        progress,
        inventory: RefCell::new(None),
        release_data: RefCell::new(Vec::new()),
        busy: Cell::new(false),
        syncing: Cell::new(false),
        revision: Cell::new(0),
        cancel_flag: RefCell::new(None),
        notice: RefCell::new(None),
    });
    root.connect_destroy({
        let owner = RefCell::new(Some(page.clone()));
        move |_| {
            if let Some(page) = owner.borrow_mut().take()
                && let Some(flag) = page.cancel_flag.borrow().as_ref()
            {
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
    page.refresh.connect_clicked({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.reload();
            }
        }
    });
    custom.connect_clicked({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.add_target();
            }
        }
    });
    page.target.connect_selected_notify({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade()
                && !page.syncing.get()
            {
                page.clear_releases();
                page.render_inventory();
            }
        }
    });
    page.provider.connect_selected_notify({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade()
                && !page.syncing.get()
            {
                page.clear_releases();
            }
        }
    });
    page.search.connect_search_changed({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.render_inventory();
            }
        }
    });
    page.fetch.connect_clicked({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.fetch_releases();
            }
        }
    });
    page.releases.connect_row_selected({
        let weak = Rc::downgrade(&page);
        move |_, _| {
            if let Some(page) = weak.upgrade()
                && !page.syncing.get()
            {
                page.show_release();
            }
        }
    });
    page.install.connect_clicked({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.install_selected();
            }
        }
    });
    page.cancel.connect_clicked({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade()
                && let Some(flag) = page.cancel_flag.borrow().as_ref()
            {
                flag.store(true, Ordering::Relaxed);
                page.status
                    .set_label("Cancelling download; staged files will be removed.");
            }
        }
    });
    let responsive = adw::BreakpointBin::builder()
        .child(&root)
        .width_request(340)
        .height_request(480)
        .build();
    let breakpoint = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 650sp").expect("valid condition"),
    );
    breakpoint.add_setter(
        &panes,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    responsive.add_breakpoint(breakpoint);
    page.reload();
    responsive.upcast()
}

impl Page {
    fn selected_target(&self) -> Option<Target> {
        self.inventory
            .borrow()
            .as_ref()?
            .targets
            .get(self.target.selected() as usize)
            .cloned()
    }
    fn selected_release(&self) -> Option<Release> {
        let row = self.releases.selected_row()?;
        self.release_data
            .borrow()
            .get(row.index() as usize)
            .cloned()
    }
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        for widget in [
            self.target.clone().upcast::<gtk::Widget>(),
            self.provider.clone().upcast(),
            self.refresh.clone().upcast(),
            self.fetch.clone().upcast(),
        ] {
            widget.set_sensitive(!busy);
        }
        self.show_release();
    }
    fn clear_releases(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        let syncing = self.syncing.replace(true);
        self.releases.unselect_all();
        self.release_data.borrow_mut().clear();
        self.repair.set_active(false);
        // ListBox owns a selection model: unparenting its children bypasses
        // that model and leaves stale row/selection state when providers change.
        while let Some(row) = self.releases.row_at_index(0) {
            self.releases.remove(&row);
        }
        self.syncing.set(syncing);
        self.show_release();
    }
    fn reload(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let selected = self.selected_target().map(|t| t.id);
        self.set_busy(true);
        self.status
            .set_label("Detecting launchers, installed tools and Steam game assignments…");
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(inventory).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(data) => {
                        let names: Vec<_> = data.targets.iter().map(|t| t.name.as_str()).collect();
                        let model = gtk::StringList::new(&names);
                        let index = selected
                            .and_then(|id| data.targets.iter().position(|t| t.id == id))
                            .unwrap_or(0);
                        let message = if data.warnings.is_empty() {
                            format!(
                                "{} launcher targets · {} custom runners · {} Steam-managed tools · {} installed Steam apps. Changes require an explicit preview and approval.",
                                data.targets.len(),
                                data.tools.len(),
                                data.steam.steam_tools.len(),
                                data.steam.games.len()
                            )
                        } else {
                            format!("Partial inventory: {}", data.warnings.join("\n"))
                        };
                        page.syncing.set(true);
                        *page.inventory.borrow_mut() = Some(data);
                        page.target.set_model(Some(&model));
                        page.target.set_selected(index as u32);
                        page.syncing.set(false);
                        if let Some(notice) = page.notice.borrow_mut().take() {
                            page.status.set_label(&format!("{notice}\n{message}"));
                        } else {
                            page.status.set_label(&message);
                        }
                        page.render_inventory();
                    }
                    Err(e) => page
                        .status
                        .set_label(&format!("Could not read compatibility tools: {e:#}")),
                }
                page.set_busy(false);
            }
        });
    }
    fn fetch_releases(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let Some(target) = self.selected_target() else {
            self.status.set_label(
                "No launcher target found. Add a target or install a supported launcher.",
            );
            return;
        };
        let Some(provider) = self.providers.get(self.provider.selected() as usize) else {
            return;
        };
        if !provider.targets.contains(&target.kind) {
            self.status.set_label("This provider does not supply runners for the selected launcher type. Select a compatible launcher/provider pair.");
            return;
        }
        let id = provider.id.to_owned();
        self.clear_releases();
        let revision = self.revision.get();
        self.set_busy(true);
        self.status.set_label("Reading upstream release metadata…");
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(move || proton::releases(&id, &target)).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if page.revision.get() != revision {
                    page.set_busy(false);
                    return;
                }
                match result {
                    Ok(releases) => {
                        let syncing = page.syncing.replace(true);
                        for release in &releases {
                            let row = adw::ActionRow::builder().use_markup(false).build();
                            row.set_title(&release.version);
                            row.set_subtitle(&format!(
                                "{}{}",
                                release.published,
                                if release.prerelease {
                                    " · prerelease"
                                } else {
                                    ""
                                }
                            ));
                            page.releases.append(&row);
                        }
                        let count = releases.len();
                        *page.release_data.borrow_mut() = releases;
                        page.syncing.set(syncing);
                        page.status.set_label(&format!("{count} upstream releases. Read notes and availability before installing; upstream tools execute as your user when selected by a launcher."));
                        if let Some(row) = page.releases.row_at_index(0) {
                            page.releases.select_row(Some(&row));
                        }
                    }
                    Err(e) => page
                        .status
                        .set_label(&format!("Could not load releases: {e:#}")),
                }
                page.set_busy(false);
            }
        });
    }
    fn show_release(&self) {
        let Some(release) = self.selected_release() else {
            self.install.set_sensitive(false);
            self.repair.set_visible(false);
            self.release_title.set_label("Select a release");
            self.release_info.set_label("Load releases for the selected launcher and provider. Official Proton is installed and updated by Steam.");
            self.notes.set_text("");
            self.release_link.set_visible(false);
            return;
        };
        self.release_title.set_label(&release.version);
        self.release_info
            .set_label(&format!("{}\n{}", release.published, release.availability));
        self.notes.set_text(if release.notes.is_empty() {
            "No release notes supplied by the publisher."
        } else {
            &release.notes
        });
        self.release_link.set_uri(&release.url);
        self.release_link.set_visible(true);
        self.install.set_label(if release.steam_app.is_some() {
            "Install / manage in Steam"
        } else {
            "Download and install"
        });
        self.install.set_sensitive(
            !self.busy.get() && (release.artifact.is_some() || release.steam_app.is_some()),
        );
        self.repair.set_visible(
            release.artifact.is_some() && !release.id.starts_with("steamtinkerlaunch:"),
        );
        self.repair.set_sensitive(!self.busy.get());
    }
    fn install_selected(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let Some(release) = self.selected_release() else {
            return;
        };
        let Some(target) = self.selected_target() else {
            return;
        };
        if let Some(app) = release.steam_app {
            glib::spawn_future_local({
                let weak = Rc::downgrade(self);
                async move {
                    let result = rt::blocking(move || proton::steam_install(&target, app)).await;
                    if let Some(page) = weak.upgrade()
                        && let Err(error) = result
                    {
                        page.ctx
                            .error("Could not open selected Steam installation", &error);
                    }
                }
            });
            return;
        }
        let Some(artifact) = release.artifact.as_ref() else {
            return;
        };
        let options = proton::InstallOptions {
            repair_prefix_parser: self.repair.is_active(),
        };
        let body = format!(
            "Install {} into {}?\n\nPublisher asset: {}\nDownload size: {}\nDestination: {}\n\nCompatibility tools are executable software from the selected upstream publisher. Existing versions are not overwritten. Restart the launcher after installation; no games are reassigned.{}",
            release.version,
            target.name,
            artifact.name,
            format_size(artifact.size),
            target.root.display(),
            if options.repair_prefix_parser {
                format!("\n\nParser repair requested:\n{}", proton_repair::WARNING)
            } else {
                String::new()
            }
        );
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if !page
                    .ctx
                    .confirm(
                        "Install compatibility tool?",
                        &body,
                        "Download and install",
                        false,
                    )
                    .await
                {
                    return;
                }
                if page.busy.get() {
                    return;
                }
                page.download(target, release, options);
            }
        });
    }
    fn download(
        self: &Rc<Self>,
        target: Target,
        release: Release,
        options: proton::InstallOptions,
    ) {
        self.set_busy(true);
        self.progress.set_visible(true);
        self.progress.set_fraction(0.);
        self.cancel.set_visible(true);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel_flag.borrow_mut() = Some(cancel.clone());
        let (tx, rx) = async_channel::bounded(8);
        rt::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                let result =
                    proton::install(&target, &release, &options, &cancel, &mut |progress| {
                        let _ = tx.try_send(DownloadEvent::Progress(progress));
                    });
                let _ = tx.send_blocking(DownloadEvent::Finished(
                    result.map_err(|e| format!("{e:#}")),
                ));
            })
            .await;
        });
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                while let Ok(event) = rx.recv().await {
                    let Some(page) = weak.upgrade() else {
                        break;
                    };
                    match event {
                        DownloadEvent::Progress(progress) => {
                            page.status.set_label(&progress.message);
                            if let Some(f) = progress.fraction {
                                page.progress.set_fraction(f.clamp(0., 1.));
                            } else {
                                page.progress.pulse();
                            }
                        }
                        DownloadEvent::Finished(result) => {
                            page.cancel_flag.borrow_mut().take();
                            page.progress.set_visible(false);
                            page.cancel.set_visible(false);
                            page.set_busy(false);
                            match result {
                                Ok(tool) => {
                                    page.ctx.toast(format!(
                                        "Installed {}. Restart the launcher to detect it.",
                                        tool.display_name
                                    ));
                                    page.reload();
                                }
                                Err(e) => page
                                    .status
                                    .set_label(&format!("Installation did not complete: {e}")),
                            }
                            break;
                        }
                    }
                }
            }
        });
    }
    fn render_inventory(self: &Rc<Self>) {
        ui::clear(&self.installed);
        ui::clear(&self.games);
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let target = self.selected_target();
        let query = self.search.text().to_lowercase();
        let Some(target) = target else {
            self.installed.append(&label("No supported launcher detected. Add an installation target above; its path is the tool directory, not the game prefix."));
            return;
        };
        let group=adw::PreferencesGroup::builder().title("Installed compatibility tools").description("Manage standalone tool display names and game assignments. Steam-managed packages are changed through Steam. Saved defaults and automatic selections do not prove which tool Steam will run.").build();
        let mut count = 0;
        for entry in data
            .tools
            .iter()
            .filter(|entry| entry.tool.target_id == target.id)
        {
            let tool = &entry.tool;
            let capabilities = &entry.capabilities;
            let associated: Vec<_> = data
                .steam
                .games
                .iter()
                .filter(|g| g.tool.as_deref() == Some(&tool.name) && same_steam_target(g, &target))
                .collect();
            if !format!(
                "{} {} {}",
                tool.name,
                tool.display_name,
                associated
                    .iter()
                    .map(|g| g.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .to_lowercase()
            .contains(&query)
            {
                continue;
            }
            count += 1;
            let row = adw::ExpanderRow::builder().use_markup(false).build();
            row.set_title(&tool.display_name);
            row.set_subtitle(&format!(
                "{} · {} associated games · {}",
                tool.name,
                associated.len(),
                if tool.managed {
                    "Hyprdeck-managed"
                } else {
                    "Externally managed"
                }
            ));
            row.add_row(&action_row(
                "Installed path",
                &tool.path.display().to_string(),
            ));
            let open = gtk::Button::with_label("Open folder");
            open.connect_clicked({
                let ctx = self.ctx.clone();
                let uri = gtk::gio::File::for_path(&tool.path).uri().to_string();
                move |_| open_uri(&ctx, &uri)
            });
            open.set_valign(gtk::Align::Center);
            row.add_suffix(&open);
            if target.kind == TargetKind::Steam {
                let games = gtk::Button::with_label("Manage games…");
                games.set_valign(gtk::Align::Center);
                row.add_suffix(&games);
                games.connect_clicked({
                    let weak = Rc::downgrade(self);
                    let target = target.clone();
                    let id = tool.name.clone();
                    let name = tool.display_name.clone();
                    move |_| {
                        if let Some(page) = weak.upgrade() {
                            page.tool_assignments(target.clone(), id.clone(), name.clone());
                        }
                    }
                });
            }
            let edit = gtk::Button::with_label("Edit name…");
            edit.set_valign(gtk::Align::Center);
            edit.set_sensitive(capabilities.editable);
            edit.set_tooltip_text(Some(&capabilities.reason));
            row.add_suffix(&edit);
            edit.connect_clicked({
                let weak = Rc::downgrade(self);
                let target = target.clone();
                let tool = tool.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.rename_tool(target.clone(), tool.clone());
                    }
                }
            });
            for game in &associated {
                row.add_row(&game_link(game));
            }
            if associated.is_empty() {
                row.add_row(&action_row("No saved game association found","Automatic selections and games managed outside Steam cannot be inferred from this list."));
            }
            if tool.managed && tool.path.join("proton").is_file() {
                let parser = gtk::Button::with_label("Prefix parser…");
                parser.set_valign(gtk::Align::Center);
                row.add_suffix(&parser);
                parser.connect_clicked({
                    let weak = Rc::downgrade(self);
                    let target = target.clone();
                    let tool = tool.clone();
                    move |_| {
                        if let Some(page) = weak.upgrade() {
                            page.parser_dialog(target.clone(), tool.clone());
                        }
                    }
                });
            }
            let remove = gtk::Button::with_label("Remove…");
            remove.set_sensitive(capabilities.removable && data.warnings.is_empty());
            remove.set_tooltip_text(Some(&capabilities.reason));
            remove.set_valign(gtk::Align::Center);
            row.add_suffix(&remove);
            remove.connect_clicked({
                let weak = Rc::downgrade(self);
                let target = target.clone();
                let tool = tool.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.remove_tool(target.clone(), tool.clone());
                    }
                }
            });
            if !capabilities.editable || !capabilities.removable {
                row.add_row(&action_row("Management limits", &capabilities.reason));
            }
            group.add(&row);
        }
        if target.kind == TargetKind::Steam {
            for tool in data
                .steam
                .steam_tools
                .iter()
                .filter(|t| same_root(&t.steam_root, &target))
            {
                let associated: Vec<_> = data
                    .steam
                    .games
                    .iter()
                    .filter(|g| {
                        g.steam_root == tool.steam_root
                            && g.tool.as_deref() == Some(&tool.internal_name)
                    })
                    .collect();
                if !format!("{} {}", tool.name, tool.internal_name)
                    .to_lowercase()
                    .contains(&query)
                {
                    continue;
                }
                count += 1;
                let row = adw::ExpanderRow::builder().use_markup(false).build();
                row.set_title(&tool.name);
                row.set_subtitle(&format!(
                    "Steam-managed · {} · {} associated games",
                    if tool.internal_name.is_empty() {
                        "Internal ID not registered by Steam"
                    } else {
                        &tool.internal_name
                    },
                    associated.len()
                ));
                row.add_row(&action_row(
                    "Installed path",
                    &tool.path.display().to_string(),
                ));
                for game in associated {
                    row.add_row(&game_link(game));
                }
                let assign = gtk::Button::with_label("Manage games…");
                assign.set_valign(gtk::Align::Center);
                assign.set_sensitive(!tool.internal_name.is_empty());
                row.add_suffix(&assign);
                assign.connect_clicked({
                    let weak = Rc::downgrade(self);
                    let target = target.clone();
                    let id = tool.internal_name.clone();
                    let name = tool.name.clone();
                    move |_| {
                        if let Some(page) = weak.upgrade() {
                            page.tool_assignments(target.clone(), id.clone(), name.clone());
                        }
                    }
                });
                let uninstall = gtk::Button::with_label("Uninstall in Steam…");
                uninstall.set_valign(gtk::Align::Center);
                row.add_suffix(&uninstall);
                uninstall.connect_clicked({
                    let weak = Rc::downgrade(self);
                    let target = target.clone();
                    let app = tool.app_id;
                    move |_| {
                        if let Some(page) = weak.upgrade() {
                            page.uninstall_steam_tool(target.clone(), app);
                        }
                    }
                });
                group.add(&row);
            }
        }
        if count == 0 {
            group.add(&action_row("No matching installed tools","Download a compatible release or adjust the search. Steam-managed tools are installed through Steam."));
        }
        self.installed.append(&group);
        let game_group=adw::PreferencesGroup::builder().title("Installed Steam games and prefix recovery").description("Back up and reset only as a troubleshooting step, not routine cleanup. Close Steam, Wine and all games first. Saves, launcher state and mods can live inside a prefix; a reset may require reinstalling prerequisites or restoring saves from the backup.").build();
        let mut game_count = 0;
        for game in data
            .steam
            .games
            .iter()
            .filter(|g| same_steam_target(g, &target))
        {
            if !format!(
                "{} {} {}",
                game.name,
                game.app_id,
                game.tool.as_deref().unwrap_or("")
            )
            .to_lowercase()
            .contains(&query)
            {
                continue;
            }
            game_count += 1;
            let row = adw::ExpanderRow::builder().use_markup(false).build();
            row.set_title(&game.name);
            row.set_subtitle(&format!(
                "{} · {} · {}",
                game.app_id,
                game.tool.as_deref().unwrap_or("Automatic / unknown"),
                game.assignment
            ));
            row.add_row(&action_row("Library", &game.library.display().to_string()));
            row.add_row(&action_row(
                "Game installation",
                &game.install_path.display().to_string(),
            ));
            row.add_row(&action_row(
                "Prefix location",
                &game.compatdata.display().to_string(),
            ));
            let links = game_link(game);
            row.add_row(&links);
            let recovery = gtk::Button::with_label("Prefix backups / recovery…");
            recovery.set_valign(gtk::Align::Center);
            row.add_suffix(&recovery);
            recovery.connect_clicked({
                let weak = Rc::downgrade(self);
                let game = game.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.prefix_dialog(game.clone());
                    }
                }
            });
            let assignment = gtk::Button::with_label("Compatibility…");
            assignment.set_valign(gtk::Align::Center);
            row.add_suffix(&assignment);
            assignment.connect_clicked({
                let weak = Rc::downgrade(self);
                let target = target.clone();
                let game = game.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.game_assignment(target.clone(), game.clone());
                    }
                }
            });
            game_group.add(&row);
        }
        if game_count == 0 {
            game_group.add(&action_row("No matching Steam games","Game assignments and Steam prefix recovery are available for detected Steam libraries; Lutris, Heroic and Bottles retain their own per-game configuration."));
        }
        self.games.append(&game_group);
        if !data.steam.mappings.is_empty() {
            let mappings = adw::PreferencesGroup::builder()
                .title("Saved Steam compatibility mappings")
                .build();
            for mapping in data
                .steam
                .mappings
                .iter()
                .filter(|m| same_root(&m.steam_root, &target))
            {
                if mapping.app_id == 0 {
                    mappings.add(&action_row(
                        "Configured default",
                        &format!(
                            "{} · {}",
                            mapping.tool.as_deref().unwrap_or("Automatic"),
                            mapping.source
                        ),
                    ));
                }
            }
            self.games.append(&mappings);
        }
    }
    fn tool_choices(&self, target: &Target) -> Vec<(String, String)> {
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return Vec::new();
        };
        let mut tools = std::collections::BTreeMap::<
            String,
            (String, std::collections::BTreeSet<PathBuf>),
        >::new();
        for entry in data
            .tools
            .iter()
            .filter(|entry| entry.tool.target_id == target.id)
        {
            let tool = &entry.tool;
            let choice = tools
                .entry(tool.name.clone())
                .or_insert_with(|| (tool.display_name.clone(), Default::default()));
            choice.1.insert(tool.path.clone());
        }
        for tool in
            data.steam.steam_tools.iter().filter(|tool| {
                same_root(&tool.steam_root, target) && !tool.internal_name.is_empty()
            })
        {
            let choice = tools
                .entry(tool.internal_name.clone())
                .or_insert_with(|| (tool.name.clone(), Default::default()));
            choice.1.insert(tool.path.clone());
        }
        tools
            .into_iter()
            .filter(|(_, (_, paths))| paths.len() == 1)
            .map(|(id, (name, _))| (id, name))
            .collect()
    }

    fn game_assignment(self: &Rc<Self>, target: Target, game: Game) {
        if self.busy.get() {
            return;
        }
        let choices = self.tool_choices(&target);
        let mut names = vec!["Automatic / configured default (remove override)".to_owned()];
        names.extend(choices.iter().map(|(id, name)| format!("{name} · {id}")));
        let dialog = adw::AlertDialog::new(
            Some("Game compatibility tool"),
            Some(&format!(
                "{} · {}\n\nThis applies to the selected Steam installation and harmonizes existing local-user overrides. Steam and Wine must be fully stopped before saving. Prefixes, saves and launch options are not edited.",
                game.name, game.app_id
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Preview changes…")]);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let row = adw::ComboRow::builder()
            .title("Compatibility tool")
            .model(&gtk::StringList::new(
                &names.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .build();
        row.set_selected(
            game.tool
                .as_ref()
                .and_then(|id| choices.iter().position(|(candidate, _)| candidate == id))
                .map_or(0, |index| index as u32 + 1),
        );
        let group = adw::PreferencesGroup::new();
        group.add(&row);
        dialog.set_extra_child(Some(&group));
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if dialog.choose_future(Some(&page.ctx.window)).await != "save" {
                    return;
                }
                let tool = if row.selected() == 0 {
                    None
                } else {
                    choices
                        .get(row.selected() as usize - 1)
                        .map(|(id, _)| id.clone())
                };
                page.preview_assignments(
                    target,
                    vec![steam_assign::Change {
                        app_id: game.app_id,
                        tool,
                    }],
                    None,
                );
            }
        });
    }

    fn tool_assignments(self: &Rc<Self>, target: Target, tool_id: String, tool_name: String) {
        if self.busy.get() {
            return;
        }
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title("Manage tool game assignments")
            .content_width(760)
            .content_height(700)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.set_margin_top(12);
        body.set_margin_bottom(12);
        body.set_margin_start(12);
        body.set_margin_end(12);
        let introduction = label(&format!(
            "{tool_name} · {tool_id}\nSelect games, then assign this tool or remove their explicit overrides. Unassigning returns selected games to Steam's automatic/configured default, regardless of their current tool. Steam/Wine must be stopped to commit; prefixes and saves are never edited."
        ));
        introduction.set_selectable(false);
        body.append(&introduction);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Find games to assign or unassign…")
            .build();
        body.append(&search);
        let (scroll, content) = ui::page_scaffold();
        scroll.set_vexpand(true);
        body.append(&scroll);
        let group = adw::PreferencesGroup::new();
        content.append(&group);
        let mut selections: Vec<(u32, bool, gtk::CheckButton)> = Vec::new();
        let mut filtered = Vec::new();
        let games: Vec<_> = data
            .steam
            .games
            .iter()
            .filter(|game| same_steam_target(game, &target))
            .collect();
        for game in &games {
            let row = action_row(
                &game.name,
                &format!(
                    "{} · {} · {}",
                    game.app_id,
                    game.tool
                        .as_deref()
                        .unwrap_or("Automatic / default / unknown"),
                    game.assignment
                ),
            );
            let selected = gtk::CheckButton::new();
            selected.set_valign(gtk::Align::Center);
            row.add_prefix(&selected);
            row.set_activatable_widget(Some(&selected));
            group.add(&row);
            selections.push((game.app_id, true, selected));
            filtered.push((format!("{} {}", game.name, game.app_id).to_lowercase(), row));
        }
        let stale: std::collections::BTreeSet<_> = data
            .steam
            .mappings
            .iter()
            .filter(|mapping| {
                same_root(&mapping.steam_root, &target)
                    && mapping.app_id != 0
                    && mapping.tool.as_deref() == Some(tool_id.as_str())
                    && !games.iter().any(|game| game.app_id == mapping.app_id)
            })
            .map(|mapping| mapping.app_id)
            .collect();
        for app in stale {
            let row = action_row(
                &format!("Uninstalled Steam app {app}"),
                "Saved reference to this tool · unassignment only",
            );
            let selected = gtk::CheckButton::new();
            selected.set_valign(gtk::Align::Center);
            row.add_prefix(&selected);
            row.set_activatable_widget(Some(&selected));
            group.add(&row);
            selections.push((app, false, selected));
            filtered.push((app.to_string(), row));
        }
        if selections.is_empty() {
            group.add(&action_row(
                "No installed games or saved references",
                "Steam library manifests are required before a tool can be assigned.",
            ));
        }
        drop(inventory);
        search.connect_search_changed(move |search| {
            let query = search.text().to_lowercase();
            for (keywords, row) in &filtered {
                row.set_visible(keywords.contains(&query));
            }
        });
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let assign = gtk::Button::with_label("Assign selected…");
        assign.add_css_class("suggested-action");
        let unassign = gtk::Button::with_label("Unassign selected…");
        buttons.append(&assign);
        buttons.append(&unassign);
        body.append(&buttons);
        let selections = Rc::new(selections);
        for (button, assign) in [(&assign, true), (&unassign, false)] {
            button.connect_clicked({ let weak=Rc::downgrade(self); let target=target.clone(); let id=tool_id.clone(); let selections=selections.clone(); let dialog=dialog.downgrade(); move |_| {
                let Some(page)=weak.upgrade() else{return;};
                let selected:Vec<_>=selections.iter().filter(|(_,_,selected)|selected.is_active()).collect();
                if selected.is_empty(){page.ctx.toast("Select at least one game.");return;}
                if assign && selected.iter().any(|(_,installed,_)|!installed){page.ctx.toast("Uninstalled game references can only be unassigned; deselect them to assign this tool.");return;}
                let changes=selected.iter().map(|(app,_,_)|steam_assign::Change{app_id:*app,tool:assign.then(||id.clone())}).collect();
                page.preview_assignments(target.clone(),changes,Some(dialog.clone()));
            }});
        }
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(&self.ctx.window));
    }

    fn preview_assignments(
        self: &Rc<Self>,
        target: Target,
        changes: Vec<steam_assign::Change>,
        dialog: Option<glib::WeakRef<adw::Dialog>>,
    ) {
        if self.busy.get() {
            return;
        }
        self.set_busy(true);
        self.status
            .set_label("Preparing an exact compatibility-assignment preview…");
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(move || steam_assign::plan(&target, &changes)).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                page.set_busy(false);
                let plan = match result {
                    Ok(plan) => plan,
                    Err(error) => {
                        page.status
                            .set_label(&format!("Could not prepare assignment changes: {error:#}"));
                        return;
                    }
                };
                if plan.files.is_empty() {
                    page.ctx
                        .toast("Selected assignments already match. Nothing was changed.");
                    return;
                }
                let changes = {
                    let names = page.inventory.borrow();
                    plan.changes
                        .iter()
                        .map(|change| {
                            let name = names
                                .as_ref()
                                .and_then(|inventory| {
                                    inventory.steam.games.iter().find(|game| {
                                        game.app_id == change.app_id
                                            && game.steam_root == plan.steam_root
                                    })
                                })
                                .map_or("Uninstalled Steam app", |game| game.name.as_str());
                            format!(
                                "{name} ({}) → {}",
                                change.app_id,
                                change
                                    .tool
                                    .as_deref()
                                    .unwrap_or("automatic/configured default")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let files = plan
                    .files
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                let body = format!(
                    "Steam installation: {}\n\n{changes}\n\nConfiguration files:\n{files}\n\nThis updates the installation-wide mapping and existing local-user overrides for these games. Complete originals are backed up. Steam and Wine must be fully stopped; concurrent changes are refused. Prefixes, saves, launch options and the global default are not edited.",
                    plan.steam_root.display()
                );
                if !page
                    .ctx
                    .confirm(
                        "Apply game compatibility assignments?",
                        &body,
                        "Apply assignments",
                        false,
                    )
                    .await
                    || page.busy.get()
                {
                    return;
                }
                page.set_busy(true);
                let result = rt::blocking(move || steam_assign::apply(&plan)).await;
                page.set_busy(false);
                match result {
                    Ok(outcome) => {
                        let backups = outcome
                            .backups
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        *page.notice.borrow_mut() = Some(format!(
                            "Updated {} game assignment(s). Original config backups:\n{backups}",
                            outcome.changed_games
                        ));
                        page.ctx.toast(format!(
                            "Updated {} game assignment(s); originals backed up.",
                            outcome.changed_games
                        ));
                        if let Some(dialog) = dialog.and_then(|dialog| dialog.upgrade()) {
                            dialog.close();
                        }
                        page.reload();
                    }
                    Err(error) => page
                        .status
                        .set_label(&format!("Assignments did not complete: {error:#}")),
                }
            }
        });
    }

    fn rename_tool(self: &Rc<Self>, target: Target, tool: InstalledTool) {
        if self.busy.get() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Edit compatibility tool display name"),
            Some(
                "Only the display name changes. The internal tool ID, installation path, version, parser state and game assignments stay unchanged. Steam/the launcher and Wine must be stopped. The original manifest is backed up.",
            ),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save display name")]);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let group = adw::PreferencesGroup::new();
        let name = adw::EntryRow::builder()
            .title("Display name")
            .text(&tool.display_name)
            .build();
        group.add(&name);
        dialog.set_extra_child(Some(&group));
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if dialog.choose_future(Some(&page.ctx.window)).await != "save" || page.busy.get() {
                    return;
                }
                let name = name.text().to_string();
                page.set_busy(true);
                let result = rt::blocking(move || proton::rename(&target, &tool, &name)).await;
                page.set_busy(false);
                match result {
                    Ok(tool) => {
                        *page.notice.borrow_mut() = Some(format!(
                            "Display name updated to {}. Internal ID and assignments unchanged; original manifest preserved.",
                            tool.display_name
                        ));
                        page.reload();
                    }
                    Err(error) => page
                        .status
                        .set_label(&format!("Could not edit tool: {error:#}")),
                }
            }
        });
    }

    fn uninstall_steam_tool(self: &Rc<Self>, target: Target, app: u32) {
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if !page.ctx.confirm("Open Steam's tool uninstall confirmation?","This tool is managed by Steam. Hyprdeck will not delete its package files. Steam handles the uninstall and any runtime dependencies. Unassign games first if you no longer want them to select this tool.","Open Steam",true).await{return;}
                let result = rt::blocking(move || proton::steam_uninstall(&target, app)).await;
                if let Err(error) = result {
                    page.ctx.error("Could not open Steam uninstall", &error);
                }
            }
        });
    }

    fn parser_dialog(self: &Rc<Self>, target: Target, tool: InstalledTool) {
        let dialog = adw::Dialog::builder()
            .title("Managed runner prefix parser")
            .content_width(700)
            .content_height(500)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let (root, content) = ui::page_scaffold();
        toolbar.set_content(Some(&root));
        dialog.set_child(Some(&toolbar));
        content.append(&label(&tool.display_name));
        content.append(&label(proton_repair::WARNING));
        let status = label("Inspecting the installed source as data; no runner code is executed…");
        content.append(&status);
        let apply = gtk::Button::with_label("Apply verified parser repair…");
        let restore = gtk::Button::with_label("Restore original upstream parser…");
        apply.set_sensitive(false);
        restore.set_sensitive(false);
        content.append(&apply);
        content.append(&restore);
        for (button, restore) in [(&apply, false), (&restore, true)] {
            button.connect_clicked({
                let weak = Rc::downgrade(self);
                let target = target.clone();
                let tool = tool.clone();
                let dialog = dialog.downgrade();
                let status = status.clone();
                move |_| {
                    let Some(page) = weak.upgrade() else {
                        return;
                    };
                    page.change_parser(
                        target.clone(),
                        tool.clone(),
                        restore,
                        dialog.clone(),
                        status.clone(),
                    );
                }
            });
        }
        glib::spawn_future_local({
            let target = target.clone();
            let tool = tool.clone();
            async move {
                match rt::blocking(move || proton_repair::inspect(&target, &tool)).await {
                    Ok(proton_repair::Status::Available { version }) => {
                        status.set_label(&format!("Recognized affected parser · prefix format {version}. Repair is optional; the original source will be preserved."));
                        apply.set_sensitive(true);
                    }
                    Ok(proton_repair::Status::Applied { version }) => {
                        status.set_label(&format!("Repair is applied · prefix format {version}. The original upstream source is preserved in the tool directory."));
                        restore.set_sensitive(true);
                    }
                    Ok(proton_repair::Status::Restored { version }) => {
                        status.set_label(&format!("Original upstream parser restored · prefix format {version}. The preserved source remains available."));
                        apply.set_sensitive(true);
                    }
                    Ok(proton_repair::Status::NotApplicable { reason }) => {
                        status.set_label(&reason)
                    }
                    Err(error) => status
                        .set_label(&format!("Could not safely inspect this runner: {error:#}")),
                }
            }
        });
        dialog.present(Some(&self.ctx.window));
    }

    fn change_parser(
        self: &Rc<Self>,
        target: Target,
        tool: InstalledTool,
        restore: bool,
        dialog: glib::WeakRef<adw::Dialog>,
        status: gtk::Label,
    ) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else {
                return;
            };
            let heading = if restore {
                "Restore original upstream parser?"
            } else {
                "Apply verified parser repair?"
            };
            let body = format!(
                "{}\n{}\n\n{}\n\nClose the launcher and all Wine/Proton games before changing an installed runner.",
                tool.display_name,
                tool.path.display(),
                proton_repair::WARNING
            );
            if !page
                .ctx
                .confirm(
                    heading,
                    &body,
                    if restore {
                        "Restore parser"
                    } else {
                        "Repair parser"
                    },
                    true,
                )
                .await
                || page.busy.get()
            {
                return;
            }
            page.set_busy(true);
            let result = rt::blocking(move || {
                if restore {
                    proton_repair::restore(&target, &tool)
                } else {
                    proton_repair::apply(&target, &tool)
                }
            })
            .await;
            page.set_busy(false);
            match result {
                Ok(proton_repair::Status::NotApplicable { reason }) => status.set_label(&reason),
                Ok(_) => {
                    page.ctx.toast(if restore { "Original upstream parser restored. Prefix migrations already performed are not undone." } else { "Verified parser repair applied. No prefix or game selection was modified." });
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                    page.reload();
                }
                Err(error) => {
                    status.set_label(&format!("Parser change did not complete: {error:#}"))
                }
            }
        });
    }
    fn remove_tool(self: &Rc<Self>, target: Target, tool: InstalledTool) {
        if self.busy.get() {
            return;
        }
        let references: std::collections::BTreeSet<_> = self
            .inventory
            .borrow()
            .as_ref()
            .map(|inventory| {
                inventory
                    .steam
                    .mappings
                    .iter()
                    .filter(|mapping| {
                        same_root(&mapping.steam_root, &target)
                            && mapping.tool.as_deref() == Some(tool.name.as_str())
                    })
                    .map(|mapping| mapping.app_id)
                    .collect()
            })
            .unwrap_or_default();
        let warning = if references.is_empty() {
            "No saved references were found. Automatic selections and other launchers cannot be guaranteed.".to_owned()
        } else if references.contains(&0) {
            "This tool is the configured Steam default. Choose another default in Steam before removing it. Per-game references can be cleared through Manage games.".to_owned()
        } else {
            format!(
                "{} saved game reference(s) must be unassigned first. Choose Manage games, clear those overrides, then return to remove the tool.",
                references.len()
            )
        };
        let body = format!(
            "Remove {} at {}?\n\n{}\n\nThis permanently removes the user-owned standalone runner directory, including any saved parser/manifest backups inside it, but never game prefixes or saves. Close Steam/the launcher and Wine first.",
            tool.display_name,
            tool.path.display(),
            warning
        );
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                let dialog = adw::AlertDialog::new(Some("Remove compatibility tool?"), Some(&body));
                dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
                dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                dialog.set_response_enabled("remove", references.is_empty());
                if target.kind == TargetKind::Steam && references.iter().any(|id| *id != 0) {
                    dialog.add_response("manage", "Manage games…");
                }
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                match dialog.choose_future(Some(&page.ctx.window)).await.as_str() {
                    "manage" => {
                        page.tool_assignments(target, tool.name, tool.display_name);
                        return;
                    }
                    "remove" => {}
                    _ => return,
                }
                if page.busy.get() {
                    return;
                }
                page.set_busy(true);
                let result = rt::blocking(move || proton::remove(&target, &tool)).await;
                page.set_busy(false);
                match result {
                    Ok(()) => {
                        *page.notice.borrow_mut()=Some("Standalone compatibility tool removed. Game prefixes and saves were not touched.".into());
                        page.reload();
                    }
                    Err(e) => page
                        .status
                        .set_label(&format!("Could not remove tool: {e:#}")),
                }
            }
        });
    }
    fn prefix_dialog(self: &Rc<Self>, game: Game) {
        let dialog = adw::Dialog::builder()
            .title(format!("Prefix recovery · {}", game.name))
            .content_width(720)
            .content_height(650)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let (root, content) = ui::page_scaffold();
        toolbar.set_content(Some(&root));
        dialog.set_child(Some(&toolbar));
        content.append(&label(recovery::RECOVERY_WARNING));
        let identity = adw::PreferencesGroup::new();
        content.append(&identity);
        identity.add(&action_row(
            "Game",
            &format!("{} · {}", game.name, game.app_id),
        ));
        identity.add(&action_row(
            "Current tool",
            game.tool.as_deref().unwrap_or("Automatic / unknown"),
        ));
        identity.add(&action_row(
            "Prefix",
            &game.compatdata.display().to_string(),
        ));
        let version = gtk::Label::builder()
            .wrap(true)
            .xalign(0.)
            .selectable(true)
            .build();
        content.append(&version);
        let result_label = label("Reading backups and checking whether Steam/Wine are stopped…");
        content.append(&result_label);
        let reset = gtk::Button::with_label("Back up and reset prefix…");
        reset.add_css_class("destructive-action");
        reset.set_sensitive(false);
        content.append(&reset);
        let backup_group = adw::PreferencesGroup::builder()
            .title("Available backups")
            .build();
        content.append(&backup_group);
        reset.connect_clicked({
            let weak = Rc::downgrade(self);
            let game = game.clone();
            let dialog = dialog.downgrade();
            let status = result_label.clone();
            move |_| {
                if let Some(page) = weak.upgrade()
                    && let Some(dialog) = dialog.upgrade()
                {
                    page.recover_prefix(game.clone(), None, &dialog, &status);
                }
            }
        });
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            let game = game.clone();
            let dialog = dialog.downgrade();
            async move {
                let info = rt::blocking({
                    let game = game.clone();
                    move || {
                        let backups = recovery::backups(&game)?;
                        let stopped = recovery::ensure_stopped(&game).map_err(|e| format!("{e:#}"));
                        let prefix_version = recovery::prefix_version(&game)?.unwrap_or_default();
                        Ok::<_, anyhow::Error>((backups, stopped, prefix_version))
                    }
                })
                .await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                match info {
                    Ok((backups, stopped, prefix_version)) => {
                        version.set_label(&format!("Prefix version: {}. Switching Proton versions may migrate built-in files and registry state; a reset is not a guaranteed fix for a runner regression.",if prefix_version.trim().is_empty(){"not initialized"}else{prefix_version.trim()}));
                        match stopped {
                            Ok(()) => {
                                reset.set_sensitive(true);
                                result_label.set_label("Steam and Wine appear stopped. This action preserves the whole compatdata directory; no backup is deleted.");
                            }
                            Err(e) => result_label.set_label(&format!("Recovery is blocked: {e}")),
                        }
                        if backups.is_empty() {
                            backup_group.add(&action_row("No backups found","Back up and reset creates a uniquely named sibling directory in the same library."));
                        }
                        for backup in backups {
                            let row = action_row(
                                &backup.id,
                                &format!("{} · created {}", backup.path.display(), backup.created),
                            );
                            let restore = gtk::Button::with_label("Restore…");
                            restore.set_valign(gtk::Align::Center);
                            row.add_suffix(&restore);
                            restore.connect_clicked({
                                let weak = Rc::downgrade(&page);
                                let game = game.clone();
                                let dialog = dialog.clone();
                                let status = result_label.clone();
                                move |_| {
                                    if let Some(page) = weak.upgrade()
                                        && let Some(dialog) = dialog.upgrade()
                                    {
                                        page.recover_prefix(
                                            game.clone(),
                                            Some(backup.clone()),
                                            &dialog,
                                            &status,
                                        );
                                    }
                                }
                            });
                            backup_group.add(&row);
                        }
                    }
                    Err(e) => result_label.set_label(&format!("Could not inspect backups: {e:#}")),
                }
            }
        });
        dialog.present(Some(&self.ctx.window));
    }
    fn recover_prefix(
        self: &Rc<Self>,
        game: Game,
        backup: Option<recovery::Backup>,
        dialog: &adw::Dialog,
        status: &gtk::Label,
    ) {
        let (heading, body, accept) = if let Some(backup) = &backup {
            (
                "Restore prefix backup?",
                format!(
                    "Restore {} for {}?\n\n{}",
                    backup.path.display(),
                    game.name,
                    recovery::RECOVERY_WARNING
                ),
                "Restore backup",
            )
        } else {
            (
                "Back up and reset this game's prefix?",
                format!(
                    "{} · {}\n{}\n\n{}",
                    game.name,
                    game.app_id,
                    game.compatdata.display(),
                    recovery::RECOVERY_WARNING
                ),
                "Back up and reset",
            )
        };
        let weak = Rc::downgrade(self);
        let dialog = dialog.downgrade();
        let status = status.clone();
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else {
                return;
            };
            if !page.ctx.confirm(heading, &body, accept, true).await {
                return;
            }
            if page.busy.get() {
                status.set_label("Another tool operation is active.");
                return;
            }
            page.set_busy(true);
            let result: anyhow::Result<String> = rt::blocking(move || {
                if let Some(backup) = backup {
                    recovery::restore(&game, &backup)?;
                    Ok(
                        "Backup restored; any current prefix was preserved in another backup."
                            .to_owned(),
                    )
                } else {
                    let backup = recovery::backup_and_reset(&game)?;
                    Ok(format!(
                        "Prefix preserved at {}. Proton creates a new prefix on next launch.",
                        backup.path.display()
                    ))
                }
            })
            .await;
            page.set_busy(false);
            match result {
                Ok(message) => {
                    page.ctx.toast(message);
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                    page.reload();
                }
                Err(error) => {
                    status.set_label(&format!("Prefix recovery did not complete: {error:#}"))
                }
            }
        });
    }
    fn add_target(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Add compatibility-tool directory"),
            Some(
                "Select the launcher format and the directory that contains compatibility tools/runners, not a game prefix. Existing files are never overwritten.",
            ),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("add", "Add target")]);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");
        let fields = adw::PreferencesGroup::new();
        let name = adw::EntryRow::builder().title("Display name").build();
        let path = adw::EntryRow::builder()
            .title("Absolute tool directory")
            .build();
        let kind = adw::ComboRow::builder()
            .title("Launcher format")
            .model(&gtk::StringList::new(&[
                "Steam", "Lutris", "Heroic", "Bottles",
            ]))
            .build();
        fields.add(&name);
        fields.add(&path);
        fields.add(&kind);
        dialog.set_extra_child(Some(&fields));
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if dialog.choose_future(Some(&page.ctx.window)).await != "add" {
                    return;
                }
                let root = PathBuf::from(path.text().as_str());
                if !root.is_absolute() || root == std::path::Path::new("/") {
                    page.ctx.toast(
                        "Choose an absolute launcher tool directory, not the filesystem root.",
                    );
                    return;
                }
                let target = CustomTarget {
                    name: if name.text().is_empty() {
                        format!(
                            "Custom {}",
                            ["Steam", "Lutris", "Heroic", "Bottles"][kind.selected() as usize]
                        )
                    } else {
                        name.text().to_string()
                    },
                    root,
                    kind: ["Steam", "Lutris", "Heroic", "Bottles"][kind.selected() as usize].into(),
                };
                let result = rt::blocking(move || {
                    let mut settings = store::load::<Settings>("compatibility-tools")?;
                    if !settings
                        .custom
                        .iter()
                        .any(|t| t.root == target.root && t.kind == target.kind)
                    {
                        settings.custom.push(target);
                    }
                    store::save("compatibility-tools", &settings)
                })
                .await;
                match result {
                    Ok(()) => page.reload(),
                    Err(e) => page.ctx.error("Could not save target", &e),
                }
            }
        });
    }
}
enum DownloadEvent {
    Progress(proton::Progress),
    Finished(Result<InstalledTool, String>),
}
fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_subtitle_lines(0);
    row
}
fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build()
}
fn game_link(game: &Game) -> adw::ActionRow {
    let row = action_row(
        &game.name,
        &format!("{} · {}", game.app_id, game.assignment),
    );
    let link = gtk::LinkButton::with_label(&game.protondb_url(), "ProtonDB");
    link.set_valign(gtk::Align::Center);
    row.add_suffix(&link);
    let steam = gtk::LinkButton::with_label(
        &format!("steam://nav/games/details/{}", game.app_id),
        "Open in Steam",
    );
    steam.set_valign(gtk::Align::Center);
    row.add_suffix(&steam);
    row
}
fn open_uri(ctx: &Ctx, uri: &str) {
    let ctx = ctx.clone();
    gtk::UriLauncher::new(uri).launch(
        Some(&ctx.window.clone()),
        gtk::gio::Cancellable::NONE,
        move |result| {
            if let Err(e) = result {
                ctx.toast(format!("Could not open link: {e}"));
            }
        },
    );
}
fn same_root(steam_root: &std::path::Path, target: &Target) -> bool {
    target
        .launcher_root
        .as_deref()
        .is_some_and(|root| root == steam_root)
}
fn same_steam_target(game: &Game, target: &Target) -> bool {
    target.kind == TargetKind::Steam && same_root(&game.steam_root, target)
}
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        "not supplied".into()
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024. * 1024.))
    }
}
