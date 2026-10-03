//! App chooser dialog: apps that declare support first, "Other application…"
//! to list everything installed, and an optional "Reset to system default".

use std::rc::Rc;

use adw::prelude::*;
use hyprdeck_core::rt;
use hyprdeck_core::ui::Ctx;

use crate::apps::{self, App};

pub enum Pick {
    App(App),
    Reset,
}

pub struct Spec {
    pub title: String,
    /// Heading above the suggested apps ("Apps that open PNG images").
    pub heading: String,
    /// Apps that declare support; empty lists every installed app right away.
    pub suggested: Vec<App>,
    /// Id of the app currently in use (marked with a check).
    pub current: Option<String>,
    pub allow_reset: bool,
    /// Show each app's command instead of its desktop id.
    pub show_commands: bool,
}

fn app_row(app: &App, current: Option<&str>, show_commands: bool) -> adw::ActionRow {
    let subtitle = if show_commands {
        app.command.as_deref().unwrap_or(&app.id)
    } else {
        app.id.as_str()
    };
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .activatable(true)
        .build();
    row.set_title(&app.name);
    row.set_subtitle(subtitle);
    row.add_prefix(&app.image(32));
    if current == Some(app.id.as_str()) {
        row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
    }
    row
}

fn list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(6)
        .build()
}

fn filter(list: &gtk::ListBox, search: &gtk::SearchEntry) {
    let s = search.clone();
    list.set_filter_func(move |row| {
        let q = s.text().to_lowercase();
        q.is_empty()
            || row.downcast_ref::<adw::ActionRow>().is_some_and(|r| {
                r.title().to_lowercase().contains(&q)
                    || r.subtitle().is_some_and(|t| t.to_lowercase().contains(&q))
            })
    });
}

pub fn open(ctx: &Ctx, spec: Spec, on_pick: impl Fn(Pick) + 'static) {
    let dialog = adw::Dialog::builder()
        .title(&spec.title)
        .content_width(560)
        .content_height(680)
        .build();
    let on_pick: Rc<dyn Fn(Pick)> = Rc::new(on_pick);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search apps")
        .hexpand(true)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);

    let add_apps = {
        let (body, dialog, on_pick, search) = (
            body.clone(),
            dialog.clone(),
            on_pick.clone(),
            search.clone(),
        );
        let (current, show_commands) = (spec.current.clone(), spec.show_commands);
        move |title: &str, apps: &[App], before: Option<&gtk::Widget>| {
            let l = list();
            for app in apps {
                let row = app_row(app, current.as_deref(), show_commands);
                let (d, f, app) = (dialog.clone(), on_pick.clone(), app.clone());
                row.connect_activated(move |_| {
                    d.close();
                    f(Pick::App(app.clone()));
                });
                l.append(&row);
            }
            filter(&l, &search);
            let s = l.clone();
            search.connect_search_changed(move |_| s.invalidate_filter());
            let h = heading(title);
            match before {
                Some(w) => {
                    h.insert_before(&body, Some(w));
                    l.insert_before(&body, Some(w));
                }
                None => {
                    body.append(&h);
                    body.append(&l);
                }
            }
        }
    };

    let actions = list();
    if spec.suggested.is_empty() {
        let (add_apps, actions, ctx) = (add_apps.clone(), actions.clone(), ctx.clone());
        ctx.spawn(async move {
            let all: Vec<App> = rt::blocking(apps::all)
                .await
                .into_iter()
                .filter(|a| a.visible)
                .collect();
            add_apps("Installed apps", &all, Some(actions.upcast_ref()));
        });
    } else {
        add_apps(&spec.heading, &spec.suggested, None);
        let other = adw::ButtonRow::builder()
            .title("Other application…")
            .start_icon_name("view-app-grid-symbolic")
            .build();
        let (add_apps, actions_c, ctx) = (add_apps.clone(), actions.clone(), ctx.clone());
        let suggested: Vec<String> = spec.suggested.iter().map(|a| a.id.clone()).collect();
        other.connect_activated(move |row| {
            row.set_visible(false);
            let (add_apps, actions, suggested) =
                (add_apps.clone(), actions_c.clone(), suggested.clone());
            ctx.spawn(async move {
                let all: Vec<App> = rt::blocking(apps::all)
                    .await
                    .into_iter()
                    .filter(|a| a.visible && !suggested.contains(&a.id))
                    .collect();
                add_apps("Other applications", &all, Some(actions.upcast_ref()));
            });
        });
        actions.append(&other);
    }
    if spec.allow_reset {
        let reset = adw::ButtonRow::builder()
            .title("Reset to system default")
            .start_icon_name("edit-undo-symbolic")
            .build();
        let (d, f) = (dialog.clone(), on_pick.clone());
        reset.connect_activated(move |_| {
            d.close();
            f(Pick::Reset);
        });
        actions.append(&reset);
    }
    actions.set_visible(actions.first_child().is_some());
    body.append(&actions);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&search));
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .child(&body)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    ));
    dialog.set_child(Some(&tv));
    dialog.present(Some(&ctx.window));
    search.grab_focus();
}
