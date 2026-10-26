//! Gaming > Games: global Hyprdeck settings, per-game overrides, runner and prefix recovery.
use super::{
    Inventory, Page, action_row, game_link, same_root, same_steam_target, variants::Origin,
};
use crate::{
    proton::{Target, TargetKind},
    steam::Game,
};
use adw::prelude::*;
use gtk::glib;
use hd_protonbuild::{
    profile::{self, Family, GameProfile, Overrides, Profiles, SETTINGS, Setting},
    variant,
};
use hyprdeck_core::{rt, ui};
use std::{cell::Cell, rc::Rc};

pub(super) struct Widgets {
    pub root: gtk::ScrolledWindow,
    pub target: adw::ComboRow,
    pub status: gtk::Label,
    pub search: gtk::SearchEntry,
    pub global: gtk::Box,
    pub list: gtk::Box,
    pub refresh: gtk::Button,
}

impl Widgets {
    pub fn new() -> Self {
        let (root, content) = ui::page_scaffold();
        let intro = adw::PreferencesGroup::builder()
            .title("Games")
            .description("Runner, Hyprdeck settings and prefix recovery for each Steam game. Settings apply when a game runs on a Hyprdeck runner; create one under Compatibility tools > Installed tools. Global defaults cover every game until you override them for one.")
            .build();
        let refresh = gtk::Button::with_label("Refresh");
        refresh.set_valign(gtk::Align::Center);
        intro.set_header_suffix(Some(&refresh));
        let target = adw::ComboRow::builder()
            .title("Steam installation")
            .visible(false)
            .build();
        intro.add(&target);
        content.append(&intro);
        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.)
            .selectable(true)
            .build();
        content.append(&status);
        let global = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&global);
        let heading = adw::PreferencesGroup::builder()
            .title("Steam games")
            .description("Open a game to override a setting. Overridden settings stop following the global defaults.")
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search games")
            .valign(gtk::Align::Center)
            .build();
        heading.set_header_suffix(Some(&search));
        content.append(&heading);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&list);
        Self {
            root,
            target,
            status,
            search,
            global,
            list,
            refresh,
        }
    }
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Ge => "GE-Proton",
        Family::CachyOs => "Proton-CachyOS",
        Family::Valve => "Valve Proton",
    }
}

/// The runner a game resolves to, and its family when it is a Hyprdeck variant.
fn runner(data: &Inventory, target: &Target, game: &Game) -> (String, Option<Family>) {
    let default = data
        .steam
        .mappings
        .iter()
        .find(|m| m.app_id == 0 && same_root(&m.steam_root, target))
        .and_then(|m| m.tool.clone());
    let (id, explicit) = match (&game.tool, default) {
        (Some(tool), _) => (tool.clone(), true),
        (None, Some(tool)) => (tool, false),
        (None, None) => return ("Steam's default runner".into(), None),
    };
    let entry = data
        .tools
        .iter()
        .find(|e| e.tool.name == id && e.tool.target_id == target.id);
    let name = entry.map_or(id.clone(), |e| e.tool.display_name.clone());
    let name = if explicit {
        name
    } else {
        format!("Steam's default, {name}")
    };
    match entry.map(|e| &e.origin) {
        Some(Origin::Variant(receipt)) => (name, Some(receipt.family)),
        _ => (name, None),
    }
}

fn summary(data: &Inventory, target: &Target, game: &Game) -> String {
    let (name, family) = runner(data, target, game);
    let mut text = match family {
        Some(_) => format!("Runs on {name} · settings apply"),
        None => format!("Runs on {name} · pick a Hyprdeck runner for settings to apply"),
    };
    match data
        .profiles
        .games
        .get(&game.app_id)
        .map_or(0, Overrides::len)
    {
        0 => {}
        1 => text.push_str(" · 1 override"),
        n => text.push_str(&format!(" · {n} overrides")),
    }
    text
}

fn on_off(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.)
        .margin_top(12)
        .margin_bottom(6)
        .margin_start(12)
        .css_classes(["heading"])
        .build()
}

fn groups() -> Vec<(&'static str, Vec<&'static Setting>)> {
    let mut groups: Vec<(&'static str, Vec<&'static Setting>)> = Vec::new();
    for setting in SETTINGS {
        match groups.last_mut() {
            Some((group, settings)) if *group == setting.group => settings.push(setting),
            _ => groups.push((setting.group, vec![setting])),
        }
    }
    groups
}

fn group_summary(profile: &GameProfile, settings: &[&Setting]) -> String {
    let on: Vec<_> = settings
        .iter()
        .filter_map(|s| match profile.flag(s.key) {
            Some(true) => Some(s.title.to_owned()),
            None if profile.frame_rate_limit > 0 => {
                Some(format!("{} FPS limit", profile.frame_rate_limit))
            }
            _ => None,
        })
        .collect();
    if on.is_empty() {
        "All off".into()
    } else {
        on.join(", ")
    }
}

impl Page {
    pub(super) fn connect_games(self: &Rc<Self>, refresh: &gtk::Button) {
        refresh.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.reload();
                }
            }
        });
        self.game_search.connect_search_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.render_game_list();
                }
            }
        });
        self.game_target.connect_selected_notify({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade()
                    && !page.syncing.get()
                {
                    page.render_games();
                }
            }
        });
    }

    fn game_target(&self) -> Option<Target> {
        self.game_targets
            .borrow()
            .get(self.game_target.selected() as usize)
            .cloned()
    }

    pub(super) fn render_games(self: &Rc<Self>) {
        let targets: Vec<Target> = self
            .inventory
            .borrow()
            .as_ref()
            .map(|data| {
                data.targets
                    .iter()
                    .filter(|t| t.kind == TargetKind::Steam && t.launcher_root.is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let ids: Vec<_> = targets.iter().map(|t| t.id.clone()).collect();
        let known: Vec<_> = self
            .game_targets
            .borrow()
            .iter()
            .map(|t| t.id.clone())
            .collect();
        if ids != known {
            let selected = self.game_target().map(|t| t.id);
            let index = selected
                .and_then(|id| ids.iter().position(|candidate| *candidate == id))
                .unwrap_or(0);
            let names: Vec<_> = targets.iter().map(|t| t.name.as_str()).collect();
            let syncing = self.syncing.replace(true);
            self.game_target
                .set_model(Some(&gtk::StringList::new(&names)));
            self.game_target.set_selected(index as u32);
            self.game_target.set_visible(targets.len() > 1);
            self.syncing.set(syncing);
            *self.game_targets.borrow_mut() = targets;
        }
        self.render_global();
        self.render_game_list();
    }

    fn render_global(self: &Rc<Self>) {
        ui::clear(&self.global);
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let Some(target) = self.game_target() else {
            return;
        };
        let group = adw::PreferencesGroup::builder()
            .title("Global defaults")
            .description("Every game on a Hyprdeck runner gets these unless it overrides them.")
            .build();
        let runners: Vec<_> = data
            .tools
            .iter()
            .filter(|e| e.tool.target_id == target.id && matches!(e.origin, Origin::Variant(_)))
            .map(|e| e.tool.display_name.as_str())
            .collect();
        group.add(&action_row(
            "Hyprdeck runners",
            &if runners.is_empty() {
                "None yet. Create one from GE-Proton or Proton-CachyOS under Compatibility tools > Installed tools.".to_owned()
            } else {
                format!(
                    "{}\nPick one per game below, or make one Steam's default in Steam > Settings > Compatibility to cover every game.",
                    runners.join(", ")
                )
            },
        ));
        let global = &data.profiles.global;
        for (name, settings) in groups() {
            let expander = adw::ExpanderRow::builder().use_markup(false).build();
            expander.set_title(name);
            expander.set_subtitle(&group_summary(global, &settings));
            for setting in settings.iter().copied() {
                let key = setting.key;
                if key == "frame_rate_limit" {
                    let spin = adw::SpinRow::with_range(0., 1000., 1.);
                    spin.set_title(setting.title);
                    spin.set_subtitle(setting.subtitle);
                    spin.set_value(f64::from(global.frame_rate_limit));
                    spin.connect_value_notify({
                        let weak = Rc::downgrade(self);
                        let expander = expander.downgrade();
                        let settings = settings.clone();
                        move |spin| {
                            let fps = spin.value().round() as u32;
                            if let Some(page) = weak.upgrade() {
                                page.update_global(
                                    move |p| p.frame_rate_limit = fps,
                                    expander.clone(),
                                    settings.clone(),
                                );
                            }
                        }
                    });
                    expander.add_row(&spin);
                } else {
                    let switch = adw::SwitchRow::builder().use_markup(false).build();
                    switch.set_title(setting.title);
                    switch.set_subtitle(setting.subtitle);
                    switch.set_active(global.flag(key).unwrap_or(false));
                    switch.connect_active_notify({
                        let weak = Rc::downgrade(self);
                        let expander = expander.downgrade();
                        let settings = settings.clone();
                        move |switch| {
                            let on = switch.is_active();
                            if let Some(page) = weak.upgrade() {
                                page.update_global(
                                    move |p| {
                                        p.set_flag(key, on);
                                    },
                                    expander.clone(),
                                    settings.clone(),
                                );
                            }
                        }
                    });
                    expander.add_row(&switch);
                }
            }
            group.add(&expander);
        }
        self.global.append(&group);
    }

    fn render_game_list(self: &Rc<Self>) {
        ui::clear(&self.games);
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let Some(target) = self.game_target() else {
            self.games.append(&action_row(
                "No Steam installation found",
                "Lutris, Heroic and Bottles keep their own per-game configuration.",
            ));
            return;
        };
        let query = self.game_search.text().to_lowercase();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let mut shown = 0;
        for game in data.steam.games.iter().filter(|g| {
            same_steam_target(g, &target)
                && format!("{} {}", g.name, g.app_id)
                    .to_lowercase()
                    .contains(&query)
        }) {
            shown += 1;
            list.append(&self.game_row(data, &target, game));
        }
        if shown > 0 {
            self.games.append(&list);
        } else {
            self.games.append(&action_row(
                "No matching Steam games",
                "Adjust the search, or install a game in Steam.",
            ));
        }
    }

    fn game_row(
        self: &Rc<Self>,
        data: &Inventory,
        target: &Target,
        game: &Game,
    ) -> adw::ExpanderRow {
        let row = adw::ExpanderRow::builder().use_markup(false).build();
        row.set_title(&game.name);
        row.set_subtitle(&summary(data, target, game));
        let runner = gtk::Button::with_label("Runner…");
        runner.set_valign(gtk::Align::Center);
        runner.set_tooltip_text(Some(
            "Choose the compatibility tool Steam uses for this game",
        ));
        row.add_suffix(&runner);
        runner.connect_clicked({
            let weak = Rc::downgrade(self);
            let target = target.clone();
            let game = game.clone();
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.game_assignment(target.clone(), game.clone());
                }
            }
        });
        let prefix = gtk::Button::with_label("Prefix…");
        prefix.set_valign(gtk::Align::Center);
        prefix.set_tooltip_text(Some("Back up, reset or restore this game's Wine prefix"));
        row.add_suffix(&prefix);
        prefix.connect_clicked({
            let weak = Rc::downgrade(self);
            let game = game.clone();
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.prefix_dialog(game.clone());
                }
            }
        });
        // Rows are built on first expand: hundreds of games times fourteen settings is slow.
        let filled = Cell::new(false);
        let app = game.app_id;
        row.connect_expanded_notify({
            let weak = Rc::downgrade(self);
            let target = target.clone();
            move |row| {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if row.is_expanded() {
                    page.expanded.borrow_mut().insert(app);
                    if !filled.replace(true) {
                        page.fill_game(row, &target, app);
                    }
                } else {
                    page.expanded.borrow_mut().remove(&app);
                }
            }
        });
        if self.expanded.borrow().contains(&app) {
            row.set_expanded(true);
        }
        row
    }

    fn fill_game(self: &Rc<Self>, row: &adw::ExpanderRow, target: &Target, app: u32) {
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let Some(game) = data
            .steam
            .games
            .iter()
            .find(|g| g.app_id == app && same_steam_target(g, target))
        else {
            return;
        };
        let (runner, family) = runner(data, target, game);
        let global = &data.profiles.global;
        let overrides = data.profiles.games.get(&app).cloned().unwrap_or_default();
        row.add_row(&action_row(
            "Runner",
            &match family {
                Some(family) => format!(
                    "{runner}, a Hyprdeck runner ({}): these settings apply",
                    family_name(family)
                ),
                None => format!(
                    "{runner}. These settings apply only on a Hyprdeck runner; choose one with Runner…"
                ),
            },
        ));
        for (name, settings) in groups() {
            row.add_row(&heading(name));
            for setting in settings {
                let key = setting.key;
                let subtitle = match family {
                    Some(f) if !GameProfile::supported(key, f) => {
                        format!("{} · not supported by {}", setting.subtitle, family_name(f))
                    }
                    _ => setting.subtitle.to_owned(),
                };
                if key == "frame_rate_limit" {
                    row.add_row(
                        &self.frame_limit_row(
                            row, target, app, setting, &subtitle, global, &overrides,
                        ),
                    );
                    continue;
                }
                let global_on = global.flag(key).unwrap_or(false);
                let choices = [
                    format!("Global default ({})", on_off(global_on)),
                    "On".to_owned(),
                    "Off".to_owned(),
                ];
                let combo = adw::ComboRow::builder()
                    .use_markup(false)
                    .model(&gtk::StringList::new(
                        &choices.iter().map(String::as_str).collect::<Vec<_>>(),
                    ))
                    .build();
                combo.set_title(setting.title);
                combo.set_subtitle(&subtitle);
                combo.set_selected(match overrides.flags.get(key) {
                    None => 0,
                    Some(true) => 1,
                    Some(false) => 2,
                });
                combo.connect_selected_notify({
                    let weak = Rc::downgrade(self);
                    let row = row.downgrade();
                    let target = target.clone();
                    move |combo| {
                        let value = match combo.selected() {
                            0 => None,
                            1 => Some(true),
                            _ => Some(false),
                        };
                        if let Some(page) = weak.upgrade() {
                            page.update_game(
                                &target,
                                app,
                                move |o| o.set_flag(key, value),
                                row.clone(),
                            );
                        }
                    }
                });
                row.add_row(&combo);
            }
        }
        row.add_row(&heading("Files"));
        row.add_row(&action_row("Library", &game.library.display().to_string()));
        row.add_row(&action_row(
            "Game installation",
            &game.install_path.display().to_string(),
        ));
        row.add_row(&action_row(
            "Prefix location",
            &game.compatdata.display().to_string(),
        ));
        row.add_row(&game_link(game));
        if !overrides.is_empty() {
            let reset = action_row(
                "Overrides",
                "Drop this game's overrides so it follows the global defaults again",
            );
            let button = gtk::Button::with_label("Reset to global defaults");
            button.set_valign(gtk::Align::Center);
            reset.add_suffix(&button);
            button.connect_clicked({
                let weak = Rc::downgrade(self);
                let target = target.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.update_game(
                            &target,
                            app,
                            |o| *o = Overrides::default(),
                            glib::WeakRef::new(),
                        );
                        page.render_game_list();
                    }
                }
            });
            row.add_row(&reset);
        }
    }

    /// A spin row that follows the global limit until edited; the check box marks an override.
    #[allow(clippy::too_many_arguments)]
    fn frame_limit_row(
        self: &Rc<Self>,
        row: &adw::ExpanderRow,
        target: &Target,
        app: u32,
        setting: &Setting,
        subtitle: &str,
        global: &GameProfile,
        overrides: &Overrides,
    ) -> adw::SpinRow {
        let spin = adw::SpinRow::with_range(0., 1000., 1.);
        spin.set_title(setting.title);
        spin.set_subtitle(&format!(
            "{subtitle} · global default {}",
            global.frame_rate_limit
        ));
        spin.set_value(f64::from(
            overrides
                .frame_rate_limit
                .unwrap_or(global.frame_rate_limit),
        ));
        let check = gtk::CheckButton::new();
        check.set_valign(gtk::Align::Center);
        check.set_tooltip_text(Some("Override the global default for this game"));
        check.set_active(overrides.frame_rate_limit.is_some());
        spin.add_prefix(&check);
        let quiet = Rc::new(Cell::new(false));
        let global_fps = global.frame_rate_limit;
        spin.connect_value_notify({
            let weak = Rc::downgrade(self);
            let row = row.downgrade();
            let target = target.clone();
            let check = check.clone();
            let quiet = quiet.clone();
            move |spin| {
                if quiet.get() {
                    return;
                }
                let fps = spin.value().round() as u32;
                quiet.set(true);
                check.set_active(true);
                quiet.set(false);
                if let Some(page) = weak.upgrade() {
                    page.update_game(
                        &target,
                        app,
                        move |o| o.frame_rate_limit = Some(fps),
                        row.clone(),
                    );
                }
            }
        });
        check.connect_toggled({
            let weak = Rc::downgrade(self);
            let row = row.downgrade();
            let target = target.clone();
            let spin = spin.clone();
            move |check| {
                if quiet.get() {
                    return;
                }
                let value = check.is_active().then(|| spin.value().round() as u32);
                if value.is_none() {
                    quiet.set(true);
                    spin.set_value(f64::from(global_fps));
                    quiet.set(false);
                }
                if let Some(page) = weak.upgrade() {
                    page.update_game(
                        &target,
                        app,
                        move |o| o.frame_rate_limit = value,
                        row.clone(),
                    );
                }
            }
        });
        spin
    }

    fn update_global(
        self: &Rc<Self>,
        change: impl FnOnce(&mut GameProfile),
        expander: glib::WeakRef<adw::ExpanderRow>,
        settings: Vec<&'static Setting>,
    ) {
        let profiles = {
            let mut data = self.inventory.borrow_mut();
            let Some(data) = data.as_mut() else {
                return;
            };
            change(&mut data.profiles.global);
            if let Some(expander) = expander.upgrade() {
                expander.set_subtitle(&group_summary(&data.profiles.global, &settings));
            }
            data.profiles.clone()
        };
        self.save_profiles(profiles, None);
        // Per-game rows show the global value; rebuild them, keeping open games open.
        self.render_game_list();
    }

    fn update_game(
        self: &Rc<Self>,
        target: &Target,
        app: u32,
        change: impl FnOnce(&mut Overrides),
        row: glib::WeakRef<adw::ExpanderRow>,
    ) {
        let profiles = {
            let mut data = self.inventory.borrow_mut();
            let Some(data) = data.as_mut() else {
                return;
            };
            let overrides = data.profiles.games.entry(app).or_default();
            change(overrides);
            if overrides.is_empty() {
                data.profiles.games.remove(&app);
            }
            data.profiles.clone()
        };
        if let Some(row) = row.upgrade()
            && let Some(data) = self.inventory.borrow().as_ref()
            && let Some(game) = data
                .steam
                .games
                .iter()
                .find(|g| g.app_id == app && same_steam_target(g, target))
        {
            row.set_subtitle(&summary(data, target, game));
        }
        self.save_profiles(profiles, Some(app));
    }

    /// Saves the global defaults (or one game's overrides) and rewrites every variant's copy.
    fn save_profiles(self: &Rc<Self>, profiles: Profiles, app: Option<u32>) {
        let mut roots: Vec<_> = self
            .game_targets
            .borrow()
            .iter()
            .map(|t| t.root.clone())
            .collect();
        roots.dedup();
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(move || -> anyhow::Result<()> {
                    let dir = profile::profiles_dir()?;
                    match app {
                        Some(app) => Profiles::save_game(
                            &dir,
                            app,
                            &profiles.games.get(&app).cloned().unwrap_or_default(),
                        )?,
                        None => Profiles::save_global(&dir, &profiles.global)?,
                    }
                    for root in roots {
                        variant::sync_profiles(&root, &profiles)?;
                    }
                    Ok(())
                })
                .await;
                if let (Err(e), Some(page)) = (result, weak.upgrade()) {
                    page.ctx.error("Could not save game settings", &e);
                }
            }
        });
    }
}
