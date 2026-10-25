//! Per-game launch settings (HDR, upscaling, latency) applied through Hyprdeck runner variants.
use crate::{
    proton::{self, InstalledTool, Target, TargetKind},
    proton_page::same_steam_target,
    steam::{self, Game},
};
use adw::prelude::*;
use gtk::glib;
use hd_protonbuild::{
    profile::{self, Family, GameProfile, SETTINGS},
    variant::{self, Receipt},
};
use hyprdeck_core::{
    rt,
    ui::{self, Ctx},
};
use std::{cell::RefCell, collections::BTreeMap, path::PathBuf, rc::Rc};

struct Data {
    target: Target,
    runners: Vec<InstalledTool>,
    variants: Vec<(PathBuf, Receipt)>,
    games: Vec<Game>,
    profiles: BTreeMap<u32, GameProfile>,
}

fn load() -> anyhow::Result<Option<Data>> {
    let mut targets = proton::targets()?;
    for target in &mut targets {
        if target.kind == TargetKind::Steam
            && let Ok(root) = proton::steam_root(target)
        {
            target.launcher_root = Some(root);
        }
    }
    let Some(target) = targets
        .iter()
        .find(|t| t.kind == TargetKind::Steam && t.launcher_root.is_some())
        .cloned()
    else {
        return Ok(None);
    };
    let variants = variant::list(&target.root)?;
    let runners = proton::installed(&target)?
        .into_iter()
        .filter(|tool| {
            tool.path.join("proton").is_file()
                && !variants.iter().any(|(path, _)| path == &tool.path)
        })
        .collect();
    let games = steam::inventory(&targets)?
        .games
        .into_iter()
        .filter(|g| same_steam_target(g, &target))
        .collect();
    let profiles = profile::load_all(&profile::profiles_dir()?)?;
    Ok(Some(Data {
        target,
        runners,
        variants,
        games,
        profiles,
    }))
}

struct Page {
    ctx: Ctx,
    status: gtk::Label,
    runners: adw::PreferencesGroup,
    runner_rows: RefCell<Vec<gtk::Widget>>,
    games: gtk::Box,
    search: gtk::SearchEntry,
    data: RefCell<Option<Data>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();
    let intro = adw::PreferencesGroup::builder()
        .title("Game settings")
        .description("HDR, upscaling and latency settings per game. They apply when the game runs on a Hyprdeck variant of a runner: a linked copy of GE-Proton or Proton-CachyOS that reads these settings at launch. The original runner is never changed, and Steam launch options still win.")
        .build();
    let refresh = gtk::Button::with_label("Refresh");
    refresh.set_valign(gtk::Align::Center);
    intro.set_header_suffix(Some(&refresh));
    content.append(&intro);
    let status = gtk::Label::builder()
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    content.append(&status);

    let runners = adw::PreferencesGroup::builder()
        .title("Runner variants")
        .description("Create a variant once per runner, then pick “&lt;runner&gt; (Hyprdeck)” for a game under Compatibility tools or in Steam.")
        .build();
    content.append(&runners);

    let games_group = adw::PreferencesGroup::builder().title("Games").build();
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search games")
        .valign(gtk::Align::Center)
        .build();
    games_group.set_header_suffix(Some(&search));
    content.append(&games_group);
    let games = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&games);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        status,
        runners,
        runner_rows: RefCell::default(),
        games,
        search,
        data: RefCell::default(),
    });
    refresh.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.reload();
            }
        }
    });
    page.search.connect_search_changed({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.render_games();
            }
        }
    });
    ui::on_shown(&scroller, {
        let page = Rc::downgrade(&page);
        move || {
            if let Some(page) = page.upgrade() {
                page.reload();
            }
        }
    });
    scroller.connect_destroy({
        let owner = RefCell::new(Some(page));
        move |_| {
            owner.borrow_mut().take();
        }
    });
    scroller.upcast()
}

impl Page {
    fn reload(self: &Rc<Self>) {
        self.status
            .set_label("Reading runners, games and saved settings…");
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(load).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(Some(data)) => {
                        page.status.set_label(&format!(
                            "{} games · {} runner variants · {} games with custom settings",
                            data.games.len(),
                            data.variants.len(),
                            data.profiles.len()
                        ));
                        *page.data.borrow_mut() = Some(data);
                    }
                    Ok(None) => {
                        page.status.set_label(
                            "No Steam installation found, so there are no games to configure.",
                        );
                        *page.data.borrow_mut() = None;
                    }
                    Err(e) => {
                        page.status
                            .set_label(&format!("Could not read games and runners: {e:#}"));
                        *page.data.borrow_mut() = None;
                    }
                }
                page.render_runners();
                page.render_games();
            }
        });
    }

    fn render_runners(self: &Rc<Self>) {
        for row in self.runner_rows.borrow_mut().drain(..) {
            self.runners.remove(&row);
        }
        let data = self.data.borrow();
        let Some(data) = data.as_ref() else {
            return;
        };
        let mut rows: Vec<gtk::Widget> = Vec::new();
        for (path, receipt) in &data.variants {
            let row = action_row(
                &receipt.display_name,
                &format!("Variant of {}", receipt.source.display()),
            );
            let refresh = gtk::Button::with_label("Rebuild");
            refresh.set_tooltip_text(Some("Relink after the original runner was updated"));
            refresh.set_valign(gtk::Align::Center);
            let remove = gtk::Button::with_label("Remove");
            remove.add_css_class("destructive-action");
            remove.set_valign(gtk::Align::Center);
            row.add_suffix(&refresh);
            row.add_suffix(&remove);
            refresh.connect_clicked({
                let page = Rc::downgrade(self);
                let source = receipt.source.clone();
                let name = receipt
                    .display_name
                    .trim_end_matches(" (Hyprdeck)")
                    .to_owned();
                move |_| {
                    if let Some(page) = page.upgrade() {
                        page.create_variant(source.clone(), name.clone());
                    }
                }
            });
            remove.connect_clicked({
                let page = Rc::downgrade(self);
                let path = path.clone();
                move |_| {
                    if let Some(page) = page.upgrade() {
                        page.remove_variant(path.clone());
                    }
                }
            });
            rows.push(row.upcast());
        }
        for tool in &data.runners {
            let row = action_row(&tool.display_name, &tool.path.display().to_string());
            let create = gtk::Button::with_label("Create variant");
            create.add_css_class("suggested-action");
            create.set_valign(gtk::Align::Center);
            row.add_suffix(&create);
            create.connect_clicked({
                let page = Rc::downgrade(self);
                let source = tool.path.clone();
                let name = tool.display_name.clone();
                move |_| {
                    if let Some(page) = page.upgrade() {
                        page.create_variant(source.clone(), name.clone());
                    }
                }
            });
            rows.push(row.upcast());
        }
        if rows.is_empty() {
            rows.push(
                action_row(
                    "No custom runners installed",
                    "Install GE-Proton or Proton-CachyOS under Compatibility tools, or build one under Proton builder.",
                )
                .upcast(),
            );
        }
        for row in &rows {
            self.runners.add(row);
        }
        *self.runner_rows.borrow_mut() = rows;
    }

    fn create_variant(self: &Rc<Self>, source: PathBuf, name: String) {
        let Some(tools_dir) = self.tools_dir() else {
            return;
        };
        let profiles = self.profiles();
        self.status
            .set_label(&format!("Creating the Hyprdeck variant of {name}…"));
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result =
                    rt::blocking(move || variant::create(&source, &tools_dir, &name, &profiles))
                        .await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(path) => page.ctx.toast(format!(
                        "Created {}. Restart Steam to see it.",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    )),
                    Err(e) => page.ctx.error("Could not create the variant", &e),
                }
                page.reload();
            }
        });
    }

    fn remove_variant(self: &Rc<Self>, path: PathBuf) {
        let page = self.clone();
        self.ctx.spawn(async move {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !page
                .ctx
                .confirm(
                    "Remove runner variant?",
                    &format!("{name} will be deleted. Games assigned to it fall back to Steam's default until you pick another runner. The original runner and your game settings are kept."),
                    "Remove",
                    true,
                )
                .await
            {
                return;
            }
            match rt::blocking(move || variant::remove(&path)).await {
                Ok(()) => page.ctx.toast(format!("Removed {name}")),
                Err(e) => page.ctx.error("Could not remove the variant", &e),
            }
            page.reload();
        });
    }

    fn render_games(self: &Rc<Self>) {
        ui::clear(&self.games);
        let data = self.data.borrow();
        let Some(data) = data.as_ref() else {
            return;
        };
        let query = self.search.text().to_lowercase();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let mut shown = 0;
        for game in data
            .games
            .iter()
            .filter(|g| g.name.to_lowercase().contains(&query))
        {
            shown += 1;
            list.append(&self.game_row(data, game));
        }
        if shown > 0 {
            self.games.append(&list);
        } else {
            self.games.append(
                &gtk::Label::builder()
                    .label("No installed Steam games match.")
                    .xalign(0.)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
    }

    fn game_row(self: &Rc<Self>, data: &Data, game: &Game) -> adw::ExpanderRow {
        let profile = data.profiles.get(&game.app_id).cloned().unwrap_or_default();
        let variant = game.tool.as_deref().and_then(|tool| {
            data.variants
                .iter()
                .find(|(path, _)| path.file_name().is_some_and(|n| n == tool))
                .map(|(_, receipt)| receipt)
        });
        let row = adw::ExpanderRow::builder().use_markup(false).build();
        row.set_title(&game.name);
        let state = match (variant, game.tool.as_deref()) {
            (Some(receipt), _) => format!("Runs on {}, settings apply", receipt.display_name),
            (None, Some(tool)) => {
                format!("Runs on {tool}; pick a Hyprdeck variant for these settings to apply")
            }
            (None, None) => {
                "Steam's default runner; pick a Hyprdeck variant for these settings to apply".into()
            }
        };
        let changed = SETTINGS
            .iter()
            .filter(|s| profile.flag(s.key) == Some(true))
            .count()
            + usize::from(profile.frame_rate_limit > 0);
        row.set_subtitle(&if changed > 0 {
            format!("{changed} settings on · {state}")
        } else {
            state
        });
        let family = variant.map(|r| r.family);
        let mut group = "";
        for setting in SETTINGS {
            if setting.group != group {
                group = setting.group;
                let heading = gtk::Label::builder()
                    .label(group)
                    .xalign(0.)
                    .margin_top(12)
                    .margin_bottom(6)
                    .margin_start(12)
                    .css_classes(["heading"])
                    .build();
                row.add_row(&heading);
            }
            let unsupported = family.filter(|f| !GameProfile::supported(setting.key, *f));
            let subtitle = match unsupported {
                Some(family) => format!(
                    "{} · not supported by {}",
                    setting.subtitle,
                    family_name(family)
                ),
                None => setting.subtitle.to_owned(),
            };
            let app = game.app_id;
            let key = setting.key;
            if key == "frame_rate_limit" {
                let spin = adw::SpinRow::with_range(0., 1000., 1.);
                spin.set_title(setting.title);
                spin.set_subtitle(&subtitle);
                spin.set_value(f64::from(profile.frame_rate_limit));
                spin.connect_value_notify({
                    let page = Rc::downgrade(self);
                    move |spin| {
                        if let Some(page) = page.upgrade() {
                            let fps = spin.value().round() as u32;
                            page.update(app, move |p| p.frame_rate_limit = fps);
                        }
                    }
                });
                row.add_row(&spin);
            } else {
                let switch = adw::SwitchRow::builder().use_markup(false).build();
                switch.set_title(setting.title);
                switch.set_subtitle(&subtitle);
                switch.set_active(profile.flag(key).unwrap_or(false));
                switch.connect_active_notify({
                    let page = Rc::downgrade(self);
                    move |switch| {
                        if let Some(page) = page.upgrade() {
                            let on = switch.is_active();
                            page.update(app, move |p| {
                                p.set_flag(key, on);
                            });
                        }
                    }
                });
                row.add_row(&switch);
            }
        }
        row
    }

    /// Applies `change` to a game's profile, saves it and rewrites every variant's copy.
    fn update(self: &Rc<Self>, app: u32, change: impl FnOnce(&mut GameProfile)) {
        let Some(tools_dir) = self.tools_dir() else {
            return;
        };
        let profiles = {
            let mut data = self.data.borrow_mut();
            let Some(data) = data.as_mut() else {
                return;
            };
            let profile = data.profiles.entry(app).or_default();
            change(profile);
            if profile.is_empty() {
                data.profiles.remove(&app);
            }
            data.profiles.clone()
        };
        let profile = profiles.get(&app).cloned().unwrap_or_default();
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(move || -> anyhow::Result<()> {
                    profile::save(&profile::profiles_dir()?, app, &profile)?;
                    variant::sync_profiles(&tools_dir, &profiles)
                })
                .await;
                if let (Err(e), Some(page)) = (result, weak.upgrade()) {
                    page.ctx.error("Could not save game settings", &e);
                }
            }
        });
    }

    fn tools_dir(&self) -> Option<PathBuf> {
        self.data.borrow().as_ref().map(|d| d.target.root.clone())
    }

    fn profiles(&self) -> BTreeMap<u32, GameProfile> {
        self.data
            .borrow()
            .as_ref()
            .map(|d| d.profiles.clone())
            .unwrap_or_default()
    }
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Ge => "GE-Proton",
        Family::CachyOs => "Proton-CachyOS",
        Family::Valve => "Valve Proton",
    }
}

fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_subtitle_lines(0);
    row
}
