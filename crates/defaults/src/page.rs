//! "Default Apps" page: XDG default applications by category, Hyprland
//! launcher variables, and a lookup for any MIME type or URL scheme.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use hyprdeck_core::hypr::{ctl, model};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::apps::{self, App};
use crate::categories::{self, CATEGORIES, Category, Kind, MimeState, State, TypeHandler};
use crate::picker::{self, Pick};
use crate::terminals::{self, TerminalState};
use crate::vars::{self, LauncherVar};
use crate::{advanced, mimeapps};

struct Page {
    ctx: Ctx,
    cats: adw::PreferencesGroup,
    cat_rows: RefCell<Vec<gtk::Widget>>,
    /// Launcher variable groups (rebuilt on refresh; empty without variables).
    vars_box: gtk::Box,
    states: RefCell<Vec<(&'static Category, State)>>,
    vars: RefCell<Vec<LauncherVar>>,
    adv_search: gtk::SearchEntry,
    adv_list: gtk::ListBox,
    /// Every known type and scheme, loaded on the first search.
    adv_types: RefCell<Option<Rc<Vec<advanced::TypeInfo>>>>,
    /// Bumped per search so stale results are dropped.
    adv_gen: Cell<u32>,
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder().use_markup(false).build();
    r.set_title(title);
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    r
}

fn button(label: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build()
}

fn tilde(p: &std::path::Path) -> String {
    match p.strip_prefix(hyprdeck_core::store::home()) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();

    let cats = adw::PreferencesGroup::builder()
        .title("Default apps")
        .description(gtk::glib::markup_escape_text(&format!(
            "Which app opens links, folders and files (XDG MIME associations, saved to {}).",
            tilde(&mimeapps::user_path())
        )))
        .build();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    cats.set_header_suffix(Some(&refresh));
    content.append(&cats);

    let vars_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.append(&vars_box);

    let adv = adw::PreferencesGroup::builder()
        .title("Advanced")
        .description("Look up any file type or link scheme (for example \"markdown\", \"image/svg\" or \"magnet:\") and choose the app that opens it.")
        .build();
    let adv_search = gtk::SearchEntry::builder()
        .placeholder_text("Search file types and link schemes")
        .margin_bottom(12)
        .build();
    adv.add(&adv_search);
    let adv_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .visible(false)
        .build();
    adv.add(&adv_list);
    content.append(&adv);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        cats,
        cat_rows: RefCell::default(),
        vars_box,
        states: RefCell::default(),
        vars: RefCell::default(),
        adv_search,
        adv_list,
        adv_types: RefCell::default(),
        adv_gen: Cell::new(0),
    });

    let p = page.clone();
    refresh.connect_clicked(move |_| p.refresh());
    let p = page.clone();
    page.adv_search.connect_search_changed(move |_| p.search());
    let p = page.clone();
    ui::on_shown(&scroller, move || p.refresh());
    scroller.upcast()
}

impl Page {
    fn refresh(self: &Rc<Self>) {
        let p = self.clone();
        self.ctx.spawn(async move {
            let (states, m, vars) = rt::blocking(|| {
                let reg = categories::registered();
                let states: Vec<(&'static Category, State)> = CATEGORIES
                    .iter()
                    .map(|c| (c, categories::state(c, &reg)))
                    .collect();
                let m = model::load();
                let vars = vars::discover(&m);
                (states, m, vars)
            })
            .await;
            p.fill_categories(&states);
            p.fill_vars(&m, &vars, &states);
            *p.states.borrow_mut() = states;
            *p.vars.borrow_mut() = vars;
            p.search();
        });
    }

    // ---- System defaults -------------------------------------------------

    fn fill_categories(self: &Rc<Self>, states: &[(&'static Category, State)]) {
        for r in self.cat_rows.borrow_mut().drain(..) {
            self.cats.remove(&r);
        }
        let mut rows = Vec::new();
        for (cat, st) in states {
            let w: gtk::Widget = match st {
                State::Mime(m) => self.mime_row(cat, m).upcast(),
                State::Terminal(t) => self.terminal_row(cat, t).upcast(),
            };
            self.cats.add(&w);
            rows.push(w);
        }
        *self.cat_rows.borrow_mut() = rows;
    }

    fn mime_row(self: &Rc<Self>, cat: &'static Category, st: &MimeState) -> adw::ExpanderRow {
        let fixable = st.fixable();
        let subtitle = match &st.current {
            None => "No app set".to_owned(),
            Some(cur) if fixable.is_empty() => cur.name.clone(),
            Some(_) => format!(
                "Mixed — {}",
                st.handlers()
                    .iter()
                    .map(|(a, n)| format!("{} ({n})", a.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let r = adw::ExpanderRow::builder().use_markup(false).build();
        r.set_title(cat.label);
        r.set_subtitle(&subtitle);
        match &st.current {
            Some(app) => r.add_prefix(&app.image(32)),
            None => r.add_prefix(
                &gtk::Image::builder()
                    .icon_name(cat.icon)
                    .pixel_size(32)
                    .build(),
            ),
        }
        if let (Some(cur), false) = (&st.current, fixable.is_empty()) {
            let unify = button("Unify");
            unify.set_tooltip_text(Some(&format!(
                "Open all {} type(s) {} supports with it",
                fixable.len(),
                cur.name
            )));
            let p = self.clone();
            unify.connect_clicked(move |_| {
                p.apply(
                    format!("{} now opens every supported type", cat.label),
                    None,
                    move || categories::unify(cat),
                )
            });
            r.add_suffix(&unify);
        }
        let change = button("Change…");
        let (p, current, candidates) = (
            self.clone(),
            st.current.as_ref().map(|a| a.id.clone()),
            st.candidates.clone(),
        );
        change.connect_clicked(move |_| {
            p.choose_for_category(cat, current.clone(), candidates.clone())
        });
        r.add_suffix(&change);

        for t in &st.types {
            r.add_row(&self.type_row(t));
        }
        r
    }

    /// One MIME type with its handler; activating it changes just that type.
    fn type_row(self: &Rc<Self>, t: &TypeHandler) -> adw::ActionRow {
        let r = row(&t.description, &t.mime);
        r.set_activatable(true);
        let handler = gtk::Label::new(Some(t.handler.as_ref().map_or("None", |h| h.name.as_str())));
        handler.add_css_class("dim-label");
        r.add_suffix(&handler);
        if let Some(h) = &t.handler {
            r.add_suffix(&h.image(16));
        }
        let (p, mime, desc, current) = (
            self.clone(),
            t.mime.clone(),
            t.description.clone(),
            t.handler.as_ref().map(|h| h.id.clone()),
        );
        r.connect_activated(move |_| {
            p.choose_for_type(mime.clone(), desc.clone(), current.clone())
        });
        r
    }

    fn terminal_row(self: &Rc<Self>, cat: &'static Category, t: &TerminalState) -> adw::ActionRow {
        let r = adw::ActionRow::builder().use_markup(false).build();
        r.set_title(cat.label);
        match &t.current {
            Some(app) => r.add_prefix(&app.image(32)),
            None => r.add_prefix(
                &gtk::Image::builder()
                    .icon_name(cat.icon)
                    .pixel_size(32)
                    .build(),
            ),
        }
        if !t.launcher {
            r.set_subtitle(&format!(
                "Apps that need a terminal start it through {}, which is not installed — install it to choose a \
                 terminal here.{}",
                terminals::LAUNCHER,
                t.detected
                    .as_deref()
                    .map(|d| format!(" hyprdeck currently uses {d}."))
                    .unwrap_or_default()
            ));
            return r;
        }
        r.set_subtitle(&match (&t.current, &t.source) {
            (Some(app), Some(src)) => format!("{} · from {}", app.name, tilde(src)),
            _ => format!(
                "Automatic — {} picks an installed terminal",
                terminals::LAUNCHER
            ),
        });
        let change = button("Change…");
        let (p, current, candidates) = (
            self.clone(),
            t.current.as_ref().map(|a| a.id.clone()),
            t.candidates.clone(),
        );
        change.connect_clicked(move |_| {
            p.choose_for_category(cat, current.clone(), candidates.clone())
        });
        r.add_suffix(&change);
        r
    }

    fn choose_for_category(
        self: &Rc<Self>,
        cat: &'static Category,
        current: Option<String>,
        candidates: Vec<App>,
    ) {
        let heading = match cat.kind {
            Kind::Terminal => "Installed terminals".to_owned(),
            Kind::Mime(_) => format!("Apps for {}", cat.label.to_lowercase()),
        };
        let p = self.clone();
        picker::open(
            &self.ctx,
            picker::Spec {
                title: format!("Default {}", cat.label.to_lowercase()),
                heading,
                suggested: candidates,
                current,
                allow_reset: true,
                show_commands: false,
            },
            move |pick| match pick {
                Pick::App(app) => {
                    let id = app.id.clone();
                    p.apply(
                        format!("{} set to {}", cat.label, app.name),
                        Some((cat.key, app)),
                        move || categories::set(cat, &id),
                    )
                }
                Pick::Reset => p.apply(
                    format!("{} reset to the system default", cat.label),
                    None,
                    move || categories::reset(cat),
                ),
            },
        );
    }

    fn choose_for_type(
        self: &Rc<Self>,
        mime: String,
        description: String,
        current: Option<String>,
    ) {
        let p = self.clone();
        self.ctx.spawn(async move {
            let m = mime.clone();
            let suggested = rt::blocking(move || apps::for_type(&m)).await;
            let p2 = p.clone();
            picker::open(
                &p.ctx,
                picker::Spec {
                    title: description.clone(),
                    heading: format!("Apps that open {mime}"),
                    suggested,
                    current,
                    allow_reset: true,
                    show_commands: false,
                },
                move |pick| {
                    let mime = mime.clone();
                    match pick {
                        Pick::App(app) => {
                            let id = app.id.clone();
                            p2.apply(
                                format!("{mime} now opens with {}", app.name),
                                None,
                                move || mimeapps::write_defaults(&[&mime], &id),
                            )
                        }
                        Pick::Reset => p2.apply(
                            format!("{mime} reset to the system default"),
                            None,
                            move || mimeapps::reset_defaults(&[&mime]),
                        ),
                    }
                },
            );
        });
    }

    /// Run a change off the main thread, report it, refresh, then offer to
    /// update a launcher variable that mirrors the changed category.
    fn apply(
        self: &Rc<Self>,
        done: String,
        changed: Option<(&'static str, App)>,
        op: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        let p = self.clone();
        self.ctx.spawn(async move {
            match rt::blocking(op).await {
                Ok(()) => p.ctx.toast(done),
                Err(e) => {
                    p.ctx.error("Changing the default app", &e);
                    return;
                }
            }
            p.refresh();
            if let Some((key, app)) = changed {
                p.offer_var_sync(key, &app).await;
            }
        });
    }

    async fn offer_var_sync(self: &Rc<Self>, key: &str, app: &App) {
        let Some(cmd) = app.command.clone() else {
            return;
        };
        let var = self
            .vars
            .borrow()
            .iter()
            .find(|v| vars::category(&v.name) == Some(key) && !vars::same_command(&v.value, &cmd))
            .cloned();
        let Some(var) = var else { return };
        let body = format!(
            "{} ({}) still launches \"{}\" from {}.\n\nChange it to \"{cmd}\"?",
            var.name,
            var.binds.join(", "),
            var.value,
            var.location()
        );
        let heading = format!("Also use {} for the Hyprland keybind?", app.name);
        if self.ctx.confirm(&heading, &body, "Update", false).await {
            self.save_var(var, cmd);
        }
    }

    // ---- Launcher variables ---------------------------------------------

    fn fill_vars(
        self: &Rc<Self>,
        m: &model::ConfigModel,
        found: &[LauncherVar],
        states: &[(&'static Category, State)],
    ) {
        ui::clear(&self.vars_box);
        if found.is_empty() {
            return;
        }
        let g = adw::PreferencesGroup::builder()
            .title("Hyprland launcher variables")
            .description(
                "String variables in your Hyprland config that keybinds launch. Changes are saved in place and \
                 Hyprland reloads.",
            )
            .build();
        let notes = adw::PreferencesGroup::new();
        let mut has_notes = false;
        for var in found {
            let entry = adw::EntryRow::builder()
                .use_markup(false)
                .tooltip_text(format!("{} at {}", var.name, var.location()))
                .show_apply_button(true)
                .build();
            entry.set_title(&format!(
                "{} ({}) · {}",
                var.label(),
                var.name,
                var.binds.join(", ")
            ));
            entry.set_text(&var.value);
            let p = self.clone();
            let v = var.clone();
            entry.connect_apply(move |e| p.save_var(v.clone(), e.text().trim().to_owned()));

            // "Use system default" when the variable mirrors a category.
            let system = vars::category(&var.name).and_then(|key| system_app(states, key));
            if let Some(app) = system.filter(|a| {
                a.command
                    .as_deref()
                    .is_some_and(|c| !vars::same_command(c, &var.value))
            }) {
                let cmd = app.command.clone().unwrap_or_default();
                let use_default = gtk::Button::builder()
                    .label("Use system default")
                    .tooltip_text(format!("Set to {} ({cmd})", app.name))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat"])
                    .build();
                let (p, v) = (self.clone(), var.clone());
                use_default.connect_clicked(move |_| p.save_var(v.clone(), cmd.clone()));
                entry.add_suffix(&use_default);
            }
            let pick = gtk::Button::builder()
                .icon_name("view-app-grid-symbolic")
                .tooltip_text("Choose an installed app")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let (p, v, e) = (self.clone(), var.clone(), entry.clone());
            pick.connect_clicked(move |_| {
                let (p2, v, e) = (p.clone(), v.clone(), e.clone());
                picker::open(
                    &p.ctx,
                    picker::Spec {
                        title: format!("Choose {}", v.label().to_lowercase()),
                        heading: String::new(),
                        suggested: Vec::new(),
                        current: None,
                        allow_reset: false,
                        show_commands: true,
                    },
                    move |pick| {
                        if let Pick::App(app) = pick
                            && let Some(cmd) = app.command
                        {
                            e.set_text(&cmd);
                            p2.save_var(v.clone(), cmd);
                        }
                    },
                );
            });
            entry.add_suffix(&pick);
            g.add(&entry);

            for s in vars::shadowed(m, &var.value) {
                let opens = s
                    .active
                    .map_or("nothing", |a| a.exec.as_deref().unwrap_or(&a.action_lua));
                let from = s.active.map(|a| a.source.display()).unwrap_or_default();
                let managed = s.active.is_some_and(vars::is_managed);
                let r = row(
                    &format!("{} opens \"{opens}\", not {}", s.original.keys, var.name),
                    &format!(
                        "{} bind at {from} replaces {}{}",
                        if managed {
                            "A hyprdeck-managed"
                        } else {
                            "A later"
                        },
                        s.original.source.display(),
                        if managed {
                            " — change it on the Keybinds page"
                        } else {
                            ""
                        }
                    ),
                );
                r.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
                notes.add(&r);
                has_notes = true;
            }
        }
        self.vars_box.append(&g);
        if has_notes {
            self.vars_box.append(&notes);
        }
    }

    fn save_var(self: &Rc<Self>, var: LauncherVar, value: String) {
        let p = self.clone();
        self.ctx.spawn(async move {
            let (v, val) = (var.clone(), value.clone());
            let r = rt::blocking(move || {
                vars::write(&v, &val)?;
                ctl::reload(true)?;
                ctl::config_errors()
            })
            .await;
            match r {
                Ok(errs) if errs.is_empty() => {
                    p.ctx.toast(format!("{} set to \"{value}\"", var.name))
                }
                Ok(errs) => p.ctx.error(
                    &format!("{} saved, but Hyprland reports", var.name),
                    &anyhow::anyhow!("{}", errs.join("; ")),
                ),
                Err(e) => p.ctx.error(&format!("Saving {}", var.name), &e),
            }
            p.refresh();
        });
    }

    // ---- Advanced --------------------------------------------------------

    fn search(self: &Rc<Self>) {
        let query = self.adv_search.text().trim().to_owned();
        let generation = self.adv_gen.get().wrapping_add(1);
        self.adv_gen.set(generation);
        if query.chars().count() < 2 {
            self.adv_list.remove_all();
            self.adv_list.set_visible(false);
            return;
        }
        let p = self.clone();
        self.ctx.spawn(async move {
            let cached = p.adv_types.borrow().clone();
            let types = match cached {
                Some(t) => t,
                None => {
                    let t = Rc::new(rt::blocking(advanced::all_types).await);
                    *p.adv_types.borrow_mut() = Some(t.clone());
                    t
                }
            };
            let (matches, more) = advanced::search(&types, &query, advanced::LIMIT);
            let results = rt::blocking(move || {
                matches
                    .iter()
                    .map(|m| categories::type_handler(m))
                    .collect::<Vec<_>>()
            })
            .await;
            if p.adv_gen.get() != generation {
                return;
            }
            p.adv_list.remove_all();
            for t in &results {
                let r = p.type_row(t);
                p.adv_list.append(&r);
            }
            if results.is_empty() {
                p.adv_list.append(&row(
                    "No matching file type or link scheme",
                    "Try a MIME type (text/x-csv) or a scheme followed by a colon (magnet:)",
                ));
            }
            if more > 0 {
                let r = row(
                    &format!("{more} more match(es)"),
                    "Refine the search to see them",
                );
                r.add_css_class("dim-label");
                p.adv_list.append(&r);
            }
            p.adv_list.set_visible(true);
        });
    }
}

/// The app a category currently uses (for "Use system default").
fn system_app(states: &[(&'static Category, State)], key: &str) -> Option<App> {
    states
        .iter()
        .find(|(c, _)| c.key == key)
        .and_then(|(_, s)| match s {
            State::Mime(m) => m.current.clone(),
            State::Terminal(t) => t.current.clone(),
        })
}
