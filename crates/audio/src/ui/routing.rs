//! App Routing tab: what is playing (move it / make a rule) and the routing rules.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::ui::{Ctx, page_scaffold};

use super::{act, follow, rule_dialog};
use crate::config::Route;
use crate::engine::{self, Status};
use crate::pw::Stream;

struct Page {
    ctx: Ctx,
    playing: adw::PreferencesGroup,
    playing_rows: RefCell<Vec<gtk::Widget>>,
    rules: adw::PreferencesGroup,
    rule_rows: RefCell<Vec<gtk::Widget>>,
    /// Rendered state of the rules list; rebuilt only when it changes.
    rules_key: RefCell<String>,
    playing_key: RefCell<String>,
    last: RefCell<Option<Status>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = page_scaffold();

    let hero = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    hero.add_css_class("audio-hero");
    let icon = gtk::Image::from_icon_name("network-transmit-symbolic");
    icon.set_pixel_size(32);
    icon.add_css_class("hero-icon");
    icon.set_valign(gtk::Align::Start);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.append(
        &gtk::Label::builder()
            .label("Send one app somewhere else")
            .xalign(0.0)
            .css_classes(["title-4"])
            .build(),
    );
    text.append(
        &gtk::Label::builder()
            .label("A rule moves every stream whose executable, application name or title contains your text to the outputs you pick — one output directly, several through their own combined output. Rules apply the moment a stream starts, whether or not simultaneous output is on.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build(),
    );
    hero.append(&icon);
    hero.append(&text);
    content.append(&hero);

    let playing = adw::PreferencesGroup::builder()
        .title("Playing now")
        .build();
    content.append(&playing);

    let rules = adw::PreferencesGroup::builder()
        .title("Routing rules")
        .description("The first matching rule wins.")
        .build();
    let add = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("list-add-symbolic")
                .label("Add rule")
                .build(),
        )
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    rules.set_header_suffix(Some(&add));
    content.append(&rules);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        playing,
        playing_rows: RefCell::default(),
        rules,
        rule_rows: RefCell::default(),
        rules_key: RefCell::default(),
        playing_key: RefCell::default(),
        last: RefCell::default(),
    });
    let p = page.clone();
    add.connect_clicked(move |_| {
        if let Some(st) = p.last.borrow().as_ref() {
            rule_dialog::open(&p.ctx, st, None, None);
        }
    });
    let p = page.clone();
    follow(&scroller, move |snap| {
        let Ok(st) = snap else { return };
        p.update(st);
    });
    scroller.upcast()
}

impl Page {
    fn update(self: &Rc<Self>, st: &Status) {
        self.last.replace(Some(st.clone()));
        self.update_playing(st);
        self.update_rules(st);
    }

    fn update_playing(self: &Rc<Self>, st: &Status) {
        let key = format!(
            "{:?}|{:?}",
            st.streams
                .iter()
                .map(|s| (
                    s.index,
                    s.sink_index,
                    s.label(),
                    s.detail().to_owned(),
                    s.corked
                ))
                .collect::<Vec<_>>(),
            st.sinks
                .iter()
                .map(|s| (&s.name, &s.label, s.index))
                .collect::<Vec<_>>()
        );
        if *self.playing_key.borrow() == key {
            return;
        }
        self.playing_key.replace(key);
        for w in self.playing_rows.borrow_mut().drain(..) {
            self.playing.remove(&w);
        }
        let mut rows: Vec<gtk::Widget> = Vec::new();
        if st.streams.is_empty() {
            rows.push(
                adw::ActionRow::builder()
                    .title("Nothing is playing right now")
                    .subtitle("Start playback in any application and it shows up here.")
                    .build()
                    .upcast(),
            );
        }
        for stream in &st.streams {
            rows.push(self.stream_row(st, stream).upcast());
        }
        for r in &rows {
            self.playing.add(r);
        }
        self.playing_rows.replace(rows);
    }

    fn stream_row(self: &Rc<Self>, st: &Status, stream: &Stream) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&stream.label()))
            .build();
        row.set_title_lines(1);
        row.set_subtitle_lines(2);
        let icon = gtk::Image::from_icon_name(if stream.icon.is_empty() {
            "audio-headphones-symbolic"
        } else {
            &stream.icon
        });
        icon.set_pixel_size(24);
        row.add_prefix(&icon);
        let mut subtitle = match stream.detail() {
            "" => "Playback stream".to_owned(),
            d => d.to_owned(),
        };
        if let Some(rule) = st.config.route_for(stream) {
            subtitle.push_str(&format!(" · rule “{}”", rule.display()));
        }
        if stream.corked {
            subtitle.push_str(" · paused");
        }
        row.set_subtitle(&glib::markup_escape_text(&subtitle));

        // Move to another output.
        let names: Vec<String> = st.sinks.iter().map(|s| s.name.clone()).collect();
        let labels: Vec<&str> = st.sinks.iter().map(|s| s.label.as_str()).collect();
        let dropdown = gtk::DropDown::from_strings(&labels);
        dropdown.set_valign(gtk::Align::Center);
        dropdown.set_tooltip_text(Some("Output this app plays on"));
        if let Some(pos) = st.sinks.iter().position(|s| s.index == stream.sink_index) {
            dropdown.set_selected(pos as u32);
        }
        let (p, index) = (self.clone(), stream.index);
        dropdown.connect_selected_notify(move |d| {
            let Some(name) = names.get(d.selected() as usize).cloned() else {
                return;
            };
            act(&p.ctx, "Could not move the stream", async move {
                engine::move_stream(index, &name).await
            });
        });
        row.add_suffix(&dropdown);

        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Create a routing rule for this app")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let (p, stream) = (self.clone(), stream.clone());
        add.connect_clicked(move |_| {
            if let Some(st) = p.last.borrow().as_ref() {
                rule_dialog::open(&p.ctx, st, None, Some(&stream));
            }
        });
        row.add_suffix(&add);
        row
    }

    fn update_rules(self: &Rc<Self>, st: &Status) {
        let key = format!(
            "{:?}|{:?}",
            st.config.routes,
            st.devices()
                .map(|s| (&s.name, &s.label))
                .collect::<Vec<_>>()
        );
        if *self.rules_key.borrow() == key {
            return;
        }
        self.rules_key.replace(key);
        for w in self.rule_rows.borrow_mut().drain(..) {
            self.rules.remove(&w);
        }
        let mut rows: Vec<gtk::Widget> = Vec::new();
        if st.config.routes.is_empty() {
            rows.push(
                adw::ActionRow::builder()
                    .title("No rules yet")
                    .subtitle("Add one, or use the + button next to a playing app above.")
                    .build()
                    .upcast(),
            );
        }
        for route in &st.config.routes {
            rows.push(self.rule_row(st, route).upcast());
        }
        for r in &rows {
            self.rules.add(r);
        }
        self.rule_rows.replace(rows);
    }

    fn rule_row(self: &Rc<Self>, st: &Status, route: &Route) -> adw::ExpanderRow {
        let targets: Vec<String> = route
            .sinks
            .iter()
            .map(|n| match st.sink(n) {
                Some(s) => s.label.clone(),
                None => format!("{n} (not connected)"),
            })
            .collect();
        let row = adw::ExpanderRow::builder()
            .title(glib::markup_escape_text(route.display()))
            .subtitle(glib::markup_escape_text(&if targets.is_empty() {
                "No outputs chosen".to_owned()
            } else {
                format!("→ {}", targets.join(" + "))
            }))
            .build();
        let switch = gtk::Switch::builder()
            .active(route.enabled)
            .valign(gtk::Align::Center)
            .tooltip_text("Rule enabled")
            .build();
        let (p, id) = (self.clone(), route.id.clone());
        switch.connect_active_notify(move |sw| {
            let (id, on) = (id.clone(), sw.is_active());
            sw.set_sensitive(false);
            let sw = sw.clone();
            super::act_then(
                &p.ctx,
                "Could not change the rule",
                async move { engine::set_route_enabled(&id, on).await },
                move |_| {
                    sw.set_sensitive(true);
                },
            );
        });
        row.add_suffix(&switch);

        let detail = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&format!(
                "{} contains “{}”",
                route.match_kind.label(),
                route.match_value
            )))
            .subtitle(match route.volume {
                Some(v) => format!("Sets the stream volume to {v}%"),
                None => "Keeps the app's own volume".to_owned(),
            })
            .build();
        let edit = gtk::Button::builder()
            .label("Edit")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let remove = gtk::Button::builder()
            .label("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "destructive-action"])
            .build();
        let (p, r) = (self.clone(), route.clone());
        edit.connect_clicked(move |_| {
            if let Some(st) = p.last.borrow().as_ref() {
                rule_dialog::open(&p.ctx, st, Some(&r), None);
            }
        });
        let (p, id, name) = (self.clone(), route.id.clone(), route.display().to_owned());
        remove.connect_clicked(move |_| {
            let (p, id, name) = (p.clone(), id.clone(), name.clone());
            p.ctx.clone().spawn(async move {
                let body = "Streams it routed go back to the default output.";
                if p.ctx
                    .confirm(&format!("Remove “{name}”?"), body, "Remove", true)
                    .await
                {
                    act(&p.ctx, "Could not remove the rule", async move {
                        engine::remove_route(&id).await
                    });
                }
            });
        });
        detail.add_suffix(&edit);
        detail.add_suffix(&remove);
        row.add_row(&detail);
        row
    }
}
