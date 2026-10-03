//! "Keybinds" page: every effective bind with a readable action and its source,
//! plus hyprdeck's disabled binds, with edit / disable / restore actions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::{self, WeakRef};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::actions::Category;
use crate::binds::{self, BindItem, Disabled, Snapshot};
use crate::editor;
use crate::widgets;

pub fn build(ctx: &Ctx) -> gtk::Widget {
    widgets::ensure_css();
    let (scroller, content) = ui::page_scaffold();

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search keys or actions")
        .hexpand(true)
        .build();
    let add = gtk::Button::builder()
        .label("Add Keybind")
        .css_classes(["suggested-action", "pill"])
        .build();
    top.append(&search);
    top.append(&add);
    content.append(&top);
    let count = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    content.append(&count);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.append(&body);

    let page = Rc::new(BindsPage {
        ctx: ctx.clone(),
        body,
        count,
        search: search.downgrade(),
        rows: RefCell::default(),
        snapshot: RefCell::default(),
        loading: Cell::new(false),
    });
    let p = page.clone();
    ui::on_shown(&scroller, move || p.reload());
    let p = page.clone();
    search.connect_search_changed(move |_| p.filter());
    let p = page.clone();
    add.connect_clicked(move |_| {
        let p2 = p.clone();
        editor::open(&p.ctx, None, p.snapshot.borrow().clone(), move || {
            p2.reload()
        });
    });
    scroller.upcast()
}

/// Rows of one group with their lowercase search text.
type GroupIndex = (
    WeakRef<adw::PreferencesGroup>,
    Vec<(WeakRef<adw::ActionRow>, String)>,
);

struct BindsPage {
    ctx: Ctx,
    body: gtk::Box,
    count: gtk::Label,
    search: WeakRef<gtk::SearchEntry>,
    /// (group, rows with search haystack) for filtering.
    rows: RefCell<Vec<GroupIndex>>,
    snapshot: RefCell<Rc<Snapshot>>,
    loading: Cell<bool>,
}

impl BindsPage {
    fn reload(self: &Rc<Self>) {
        if self.loading.replace(true) {
            return;
        }
        if self.body.first_child().is_none() {
            self.body.append(&widgets::loading());
        }
        let p = self.clone();
        glib::spawn_future_local(async move {
            let res = rt::blocking(binds::snapshot).await;
            p.loading.set(false);
            ui::clear(&p.body);
            match res {
                Ok(snap) => {
                    let snap = Rc::new(snap);
                    p.snapshot.replace(snap.clone());
                    p.render(&snap);
                    p.filter();
                }
                Err(e) => {
                    let p2 = p.clone();
                    p.body.append(&widgets::error_page(
                        "Could not read keybinds",
                        &e,
                        move || p2.reload(),
                    ));
                }
            }
        });
    }

    fn render(self: &Rc<Self>, snap: &Rc<Snapshot>) {
        let mut index = Vec::new();
        if !snap.errors.is_empty() {
            let g = adw::PreferencesGroup::builder()
                .title("Config errors")
                .description("Your Lua config failed to evaluate fully; binds after the error may be missing")
                .build();
            for e in &snap.errors {
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title_selectable(true)
                    .build();
                row.set_title(e);
                row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
                g.add(&row);
            }
            self.body.append(&g);
        }
        let caps = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
        for cat in Category::ALL {
            let items: Vec<&BindItem> = snap.binds.iter().filter(|b| b.category == cat).collect();
            if items.is_empty() {
                continue;
            }
            let g = adw::PreferencesGroup::builder()
                .title(glib::markup_escape_text(cat.title()))
                .build();
            let mut rows = Vec::new();
            for item in items {
                let row = self.bind_row(item, snap, &caps);
                g.add(&row);
                let hay = format!(
                    "{} {} {} {} {}",
                    item.combo.0,
                    item.keys,
                    item.label,
                    item.detail,
                    item.description.as_deref().unwrap_or("")
                )
                .to_lowercase();
                rows.push((row.downgrade(), hay));
            }
            self.body.append(&g);
            index.push((g.downgrade(), rows));
        }
        if !snap.disabled.is_empty() {
            let g = adw::PreferencesGroup::builder()
                .title("Disabled")
                .description("Hand-written binds switched off by hyprdeck")
                .build();
            let mut rows = Vec::new();
            for d in &snap.disabled {
                let row = self.disabled_row(d, &caps);
                g.add(&row);
                let hay = format!(
                    "{} {}",
                    d.combo.0,
                    d.was
                        .iter()
                        .map(|w| w.label.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
                .to_lowercase();
                rows.push((row.downgrade(), hay));
            }
            self.body.append(&g);
            index.push((g.downgrade(), rows));
        }
        if snap.binds.is_empty() && snap.disabled.is_empty() {
            self.body.append(
                &adw::StatusPage::builder()
                    .icon_name("preferences-desktop-keyboard-shortcuts-symbolic")
                    .title("No Keybinds")
                    .description("Your Hyprland config defines no binds")
                    .build(),
            );
        }
        self.rows.replace(index);
    }

    fn filter(&self) {
        let q = self
            .search
            .upgrade()
            .map(|s| s.text().trim().to_lowercase())
            .unwrap_or_default();
        let terms: Vec<&str> = q.split_whitespace().filter(|t| *t != "+").collect();
        let mut shown = 0;
        for (g, rows) in self.rows.borrow().iter() {
            let mut any = false;
            for (row, hay) in rows {
                let vis = terms.iter().all(|t| hay.contains(t));
                if let Some(r) = row.upgrade() {
                    r.set_visible(vis);
                }
                any |= vis;
                shown += usize::from(vis);
            }
            if let Some(g) = g.upgrade() {
                g.set_visible(any);
            }
        }
        let snap = self.snapshot.borrow();
        let total = snap.binds.len() + snap.disabled.len();
        self.count.set_label(&if q.is_empty() {
            format!(
                "{} active binds, {} disabled",
                snap.binds.len(),
                snap.disabled.len()
            )
        } else {
            format!("{shown} of {total} binds match")
        });
    }

    fn bind_row(
        self: &Rc<Self>,
        item: &BindItem,
        snap: &Rc<Snapshot>,
        caps: &gtk::SizeGroup,
    ) -> adw::ActionRow {
        let mut subtitle = item.detail.clone();
        if let Some(d) = &item.description {
            subtitle = format!("{d} · {subtitle}");
        }
        if !item.submap.is_empty() {
            subtitle.push_str(&format!(" · submap {}", item.submap));
        }
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_lines(2)
            .build();
        row.set_title(&item.label);
        row.set_subtitle(&subtitle);
        let kc = widgets::keycaps(&item.combo.0);
        kc.set_width_request(200);
        caps.add_widget(&kc);
        row.add_prefix(&kc);
        for f in item.flag_labels() {
            row.add_suffix(&widgets::badge(f, None));
        }
        let source = if item.managed {
            let b = widgets::badge("hyprdeck", Some("accent"));
            let tip = if item.overrides.is_empty() {
                "Added by hyprdeck".to_owned()
            } else {
                format!(
                    "Set by hyprdeck, replacing {}",
                    item.overrides
                        .iter()
                        .map(|o| format!("“{}” ({})", o.label, o.source))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            b.set_tooltip_text(Some(&tip));
            b
        } else {
            let l = widgets::origin_label();
            l.set_label(&item.source);
            l.set_tooltip_text(Some(&format!("Defined in your config at {}", item.source)));
            l
        };
        row.add_suffix(&source);

        if item.dispatcher.is_some() {
            let edit = widgets::icon_button("document-edit-symbolic", "Edit");
            let (p, it, s) = (Rc::downgrade(self), item.clone(), snap.clone());
            edit.connect_clicked(move |_| {
                let Some(p) = p.upgrade() else { return };
                let p2 = p.clone();
                editor::open(&p.ctx, Some(it.clone()), s.clone(), move || p2.reload());
            });
            row.add_suffix(&edit);
        }
        if item.managed && !item.overrides.is_empty() {
            let b = widgets::icon_button("edit-undo-symbolic", "Restore the bind from your config");
            self.op(
                &b,
                item.keys.clone(),
                Op::RemoveManaged,
                "Restored original bind",
            );
            row.add_suffix(&b);
        }
        if item.managed && item.overrides.is_empty() {
            let b = widgets::icon_button("user-trash-symbolic", "Remove this hyprdeck bind");
            self.op(&b, item.keys.clone(), Op::RemoveManaged, "Removed bind");
            row.add_suffix(&b);
        } else {
            let b = widgets::icon_button("action-unavailable-symbolic", "Disable this shortcut");
            self.op(&b, item.keys.clone(), Op::Disable, "Disabled");
            row.add_suffix(&b);
        }
        row
    }

    fn disabled_row(self: &Rc<Self>, d: &Disabled, caps: &gtk::SizeGroup) -> adw::ActionRow {
        let title = if d.was.is_empty() {
            "No hand-written bind on this combo".to_owned()
        } else {
            d.was
                .iter()
                .map(|w| w.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_title(&title);
        row.set_subtitle(
            &d.was
                .iter()
                .map(|w| w.source.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        row.add_css_class("dim-label");
        let kc = widgets::keycaps(&d.combo.0);
        kc.set_width_request(200);
        caps.add_widget(&kc);
        row.add_prefix(&kc);
        let b = gtk::Button::with_label("Restore");
        b.set_valign(gtk::Align::Center);
        self.op(&b, d.keys.clone(), Op::Restore, "Restored");
        row.add_suffix(&b);
        row
    }

    fn op(self: &Rc<Self>, button: &gtk::Button, keys: String, op: Op, done: &'static str) {
        let p = Rc::downgrade(self);
        button.connect_clicked(move |b| {
            let Some(p) = p.upgrade() else { return };
            b.set_sensitive(false);
            let keys = keys.clone();
            let b = b.downgrade();
            glib::spawn_future_local(async move {
                if op == Op::RemoveManaged && !p.ctx.confirm(
                    "Remove hyprdeck bind?",
                    &format!("{keys} goes back to whatever your config binds there (if anything)."),
                    "Remove",
                    true,
                )
                .await
                {
                    if let Some(b) = b.upgrade() {
                        b.set_sensitive(true);
                    }
                    return;
                }
                let k = keys.clone();
                let res = rt::blocking(move || match op {
                    Op::Disable => binds::disable(&k),
                    Op::RemoveManaged => binds::remove_managed(&k),
                    Op::Restore => binds::restore(&k),
                })
                .await;
                match res {
                    Ok(errors) => {
                        widgets::report_errors(&p.ctx, &format!("{done}: {keys}"), &errors)
                    }
                    Err(e) => p.ctx.error("Could not update keybind", &e),
                }
                p.reload();
            });
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Disable,
    RemoveManaged,
    Restore,
}
