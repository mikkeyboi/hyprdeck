//! Outputs tab: status hero, default output selector, device list (group selection,
//! default, volume) and per-output sync.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::ui::{Ctx, page_scaffold};

use super::{ROUTING_TAB, VolumeControl, act, act_then, badge, follow, show_tab};
use crate::daemon;
use crate::engine::{self, OutputChoice, Status, simultaneous_label};
use crate::pw::{PRIMARY_SINK, Sink};

struct DeviceRow {
    name: String,
    row: adw::ActionRow,
    check: gtk::CheckButton,
    vol: VolumeControl,
    make_default: gtk::Button,
    star: gtk::Image,
}

struct SyncRow {
    name: String,
    row: adw::SpinRow,
    updating: Rc<Cell<bool>>,
    last_user: Rc<Cell<Option<Instant>>>,
}

struct Page {
    ctx: Ctx,
    hero: gtk::Box,
    hero_title: gtk::Label,
    hero_sub: gtk::Label,
    playing: gtk::Box,
    switch: gtk::Switch,
    switch_guard: Cell<bool>,
    default_group: adw::PreferencesGroup,
    default_row: adw::ComboRow,
    default_model: gtk::StringList,
    default_spinner: adw::Spinner,
    /// Entries behind `default_model`, same order.
    default_choices: RefCell<Vec<OutputChoice>>,
    /// The live default as of the last snapshot.
    default_current: RefCell<Option<OutputChoice>>,
    /// Set while the selection is changed programmatically.
    default_guard: Cell<bool>,
    /// A default change is being applied; snapshots leave the selector alone.
    default_busy: Cell<bool>,
    group: adw::PreferencesGroup,
    empty: adw::ActionRow,
    rows: RefCell<Vec<DeviceRow>>,
    footer: gtk::Box,
    apply: gtk::Button,
    touched: Cell<bool>,
    syncing: Cell<bool>,
    selected: RefCell<Vec<String>>,
    active: Cell<bool>,
    sync: adw::PreferencesGroup,
    sync_rows: RefCell<Vec<SyncRow>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = page_scaffold();

    // Status hero.
    let hero = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    hero.add_css_class("audio-hero");
    let icon = gtk::Image::from_icon_name("audio-speakers-symbolic");
    icon.set_pixel_size(40);
    icon.add_css_class("hero-icon");
    icon.set_valign(gtk::Align::Start);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.set_hexpand(true);
    let hero_title = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-3"])
        .label("Reading outputs…")
        .build();
    let hero_sub = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    let playing = gtk::Box::new(gtk::Orientation::Vertical, 4);
    playing.set_margin_top(6);
    text.append(&hero_title);
    text.append(&hero_sub);
    text.append(&playing);
    let routing = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("media-playlist-shuffle-symbolic")
                .label("Route apps to other outputs…")
                .build(),
        )
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .build();
    routing.connect_clicked(|_| show_tab(ROUTING_TAB));
    text.append(&routing);
    let switch = gtk::Switch::builder()
        .valign(gtk::Align::Start)
        .tooltip_text("Simultaneous output")
        .build();
    hero.append(&icon);
    hero.append(&text);
    hero.append(&switch);
    content.append(&hero);

    // Default output selector; the chosen entry shows as the row subtitle, so long
    // device names get the full width instead of an ellipsized button label.
    let default_model = gtk::StringList::new(&[]);
    let default_row = adw::ComboRow::builder()
        .use_markup(false)
        .use_subtitle(true)
        .model(&default_model)
        .sensitive(false)
        .build();
    default_row.set_title("Default output");
    let default_spinner = adw::Spinner::builder().visible(false).build();
    default_row.add_suffix(&default_spinner);
    let default_group = adw::PreferencesGroup::builder()
        .description("Where system sound goes.")
        .build();
    default_group.add(&default_row);
    content.append(&default_group);

    // Devices.
    let group = adw::PreferencesGroup::builder()
        .title("Outputs")
        .description("Tick two or more outputs to play on all of them at once. Apps claimed by a routing rule keep their own output.")
        .build();
    let pair = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("bluetooth-symbolic")
                .label("Pair a Bluetooth device…")
                .build(),
        )
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    pair.connect_clicked(|_| events::send(AppEvent::ShowPage("bluetooth".into())));
    group.set_header_suffix(Some(&pair));
    let empty = adw::ActionRow::builder()
        .title("No audio outputs detected")
        .subtitle("Check that PipeWire is running; outputs appear here as soon as it reports them.")
        .build();
    group.add(&empty);
    content.append(&group);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_halign(gtk::Align::End);
    footer.set_margin_top(-12);
    let select_all = gtk::Button::builder()
        .label("Select all")
        .css_classes(["flat", "pill"])
        .build();
    let apply = gtk::Button::builder()
        .label("Apply selection")
        .css_classes(["pill"])
        .sensitive(false)
        .build();
    footer.append(&select_all);
    footer.append(&apply);
    content.append(&footer);

    let sync = adw::PreferencesGroup::builder()
        .title("Sync")
        .description("If one output sounds late, raise its offset; PipeWire delays the other outputs to match (module-combine-sink latency compensation).")
        .visible(false)
        .build();
    content.append(&sync);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        hero,
        hero_title,
        hero_sub,
        playing,
        switch,
        switch_guard: Cell::new(false),
        default_group,
        default_row,
        default_model,
        default_spinner,
        default_choices: RefCell::default(),
        default_current: RefCell::default(),
        default_guard: Cell::new(false),
        default_busy: Cell::new(false),
        group,
        empty,
        rows: RefCell::default(),
        footer,
        apply,
        touched: Cell::new(false),
        syncing: Cell::new(false),
        selected: RefCell::default(),
        active: Cell::new(false),
        sync,
        sync_rows: RefCell::default(),
    });

    let p = page.clone();
    page.switch.connect_active_notify(move |sw| {
        if p.switch_guard.get() {
            return;
        }
        sw.set_sensitive(false);
        let sw2 = sw.clone();
        let done = move |_| sw2.set_sensitive(true);
        if sw.is_active() {
            let checked = p.checked();
            if checked.len() >= 2 {
                act_then(
                    &p.ctx,
                    "Could not turn on simultaneous output",
                    async move { engine::enable(&checked).await },
                    done,
                );
            } else {
                act_then(
                    &p.ctx,
                    "Could not turn on simultaneous output",
                    engine::toggle(),
                    done,
                );
            }
            p.touched.set(false);
        } else {
            act_then(
                &p.ctx,
                "Could not turn off simultaneous output",
                engine::disable(),
                done,
            );
        }
    });
    let p = page.clone();
    page.default_row.connect_selected_notify(move |row| {
        if p.default_guard.get() {
            return;
        }
        let Some(choice) = p
            .default_choices
            .borrow()
            .get(row.selected() as usize)
            .cloned()
        else {
            return;
        };
        if p.default_current.borrow().as_ref() == Some(&choice) {
            return;
        }
        match choice {
            OutputChoice::Simultaneous => {
                let ticked = p.checked();
                if ticked.len() < 2 {
                    p.ctx
                        .toast("Tick at least two outputs below to play on all of them at once.");
                    p.select_current_default();
                    return;
                }
                p.touched.set(false);
                p.apply_default(
                    "Could not switch to simultaneous output",
                    engine::use_simultaneous(Some(ticked)),
                );
            }
            OutputChoice::Device(name) => p
                .apply_default("Could not change the default output", async move {
                    engine::use_output(&name).await
                }),
        }
    });
    let p = page.clone();
    select_all.connect_clicked(move |_| {
        for r in p.rows.borrow().iter() {
            r.check.set_active(true);
        }
    });
    let p = page.clone();
    page.apply.connect_clicked(move |b| {
        let checked = p.checked();
        p.touched.set(false);
        b.set_sensitive(false);
        act(&p.ctx, "Could not apply the selection", async move {
            engine::set_selected(&checked).await
        });
    });

    let p = page.clone();
    follow(&scroller, move |snap| p.update(snap));
    scroller.upcast()
}

impl Page {
    fn checked(&self) -> Vec<String> {
        self.rows
            .borrow()
            .iter()
            .filter(|r| r.check.is_active())
            .map(|r| r.name.clone())
            .collect()
    }

    fn update(self: &Rc<Self>, snap: &Result<Status, String>) {
        let st = match snap {
            Ok(st) => st,
            Err(e) => {
                self.hero.add_css_class("off");
                self.hero_title.set_label("PipeWire is not available");
                self.hero_sub.set_label(e);
                self.switch.set_sensitive(false);
                self.default_row.set_sensitive(false);
                return;
            }
        };
        self.switch.set_sensitive(true);
        self.active.set(st.active);
        self.update_hero(st);
        self.update_devices(st);
        self.update_default(st);
        self.update_sync(st);
    }

    /// Mirror the live default in the selector (labels may have changed too).
    fn update_default(&self, st: &Status) {
        if self.default_busy.get() {
            // The action's completion re-syncs from the newest snapshot.
            return;
        }
        let choices = st.output_choices();
        let ticked = self.checked().len();
        let labels: Vec<String> = choices
            .iter()
            .map(|c| match c {
                OutputChoice::Device(name) => match st.sink(name) {
                    Some(s) => format!("{} ({})", s.label, s.conn.label()),
                    None => name.clone(),
                },
                OutputChoice::Simultaneous => simultaneous_label(ticked),
            })
            .collect();
        let model = &self.default_model;
        let same = model.n_items() as usize == labels.len()
            && labels
                .iter()
                .enumerate()
                .all(|(i, l)| model.string(i as u32).as_deref() == Some(l.as_str()));
        self.default_guard.set(true);
        if !same {
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            model.splice(0, model.n_items(), &refs);
        }
        self.default_choices.replace(choices);
        self.default_current.replace(st.current_choice());
        self.default_guard.set(false);
        self.select_current_default();
        self.default_row.set_sensitive(true);
        self.default_group
            .set_description(Some(&glib::markup_escape_text(&default_subtitle(st))));
    }

    /// Point the selector back at the live default without applying anything.
    fn select_current_default(&self) {
        let index = self
            .default_current
            .borrow()
            .as_ref()
            .and_then(|c| self.default_choices.borrow().iter().position(|x| x == c));
        self.default_guard.set(true);
        self.default_row
            .set_selected(index.map_or(gtk::INVALID_LIST_POSITION, |i| i as u32));
        self.default_guard.set(false);
    }

    /// Keep the simultaneous entry's device count in step with the ticks.
    fn refresh_simultaneous_label(&self) {
        let n = self.default_model.n_items();
        if n == 0 {
            return;
        }
        let label = simultaneous_label(self.checked().len());
        if self.default_model.string(n - 1).as_deref() != Some(label.as_str()) {
            let selected = self.default_row.selected();
            self.default_guard.set(true);
            self.default_model.splice(n - 1, 1, &[label.as_str()]);
            self.default_row.set_selected(selected);
            self.default_guard.set(false);
        }
    }

    /// Run a default-output change with the selector busy until it lands.
    fn apply_default<F>(self: &Rc<Self>, what: &'static str, fut: F)
    where
        F: Future<Output = anyhow::Result<String>> + Send + 'static,
    {
        self.default_busy.set(true);
        self.default_row.set_sensitive(false);
        self.default_spinner.set_visible(true);
        let p = self.clone();
        act_then(&self.ctx, what, fut, move |_| {
            p.default_busy.set(false);
            p.default_spinner.set_visible(false);
            p.default_row.set_sensitive(true);
            // Success or not, show what PipeWire now reports.
            match daemon::current().as_deref() {
                Some(Ok(st)) => p.update_default(st),
                _ => p.select_current_default(),
            }
        });
    }

    fn update_hero(&self, st: &Status) {
        self.switch_guard.set(true);
        self.switch.set_active(st.active);
        self.switch_guard.set(false);
        let default_label = st.label(&st.default_sink);
        if st.active {
            self.hero.remove_css_class("off");
            let members = st.members_of(PRIMARY_SINK);
            let names: Vec<&str> = if members.is_empty() {
                st.config.selected.iter().map(|n| st.label(n)).collect()
            } else {
                members.iter().map(|s| s.label.as_str()).collect()
            };
            self.hero_title
                .set_label(&format!("Playing on {} outputs at once", names.len()));
            let mut sub = names.join(" + ");
            if st.default_sink != PRIMARY_SINK {
                sub.push_str(&format!(
                    "\nNot the default output — new apps play on {default_label}."
                ));
            }
            self.hero_sub.set_label(&sub);
        } else {
            self.hero.add_css_class("off");
            self.hero_title.set_label("Simultaneous output is off");
            let mut sub = if st.default_sink.is_empty() {
                "No default output".to_owned()
            } else {
                format!("Sound goes to {default_label}.")
            };
            if st.config.selected.len() >= 2 {
                sub.push_str(&format!(
                    " Turning it on plays on {}.",
                    st.labels(&st.config.selected).replace(", ", " + ")
                ));
            }
            self.hero_sub.set_label(&sub);
        }

        hyprdeck_core::ui::clear(&self.playing);
        if st.streams.is_empty() {
            let l = gtk::Label::builder()
                .label("Nothing is playing right now.")
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build();
            self.playing.append(&l);
        }
        for stream in &st.streams {
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::from_icon_name(if stream.icon.is_empty() {
                "audio-x-generic-symbolic"
            } else {
                &stream.icon
            });
            icon.set_pixel_size(16);
            let dest = match st.destination(stream) {
                Some((sink, members)) if !members.is_empty() => format!(
                    "{} ({})",
                    sink.label,
                    members
                        .iter()
                        .map(|m| m.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                Some((sink, _)) => sink.label.clone(),
                None => "nowhere".to_owned(),
            };
            let mut markup = format!(
                "<b>{}</b> → {}",
                glib::markup_escape_text(&stream.label()),
                glib::markup_escape_text(&dest)
            );
            if let Some(rule) = st.config.route_for(stream) {
                markup.push_str(&format!(
                    " <span alpha='60%'>· rule “{}”</span>",
                    glib::markup_escape_text(rule.display())
                ));
            }
            if stream.corked {
                markup.push_str(" <span alpha='60%'>· paused</span>");
            }
            let label = gtk::Label::builder()
                .use_markup(true)
                .label(&markup)
                .xalign(0.0)
                .wrap(true)
                .build();
            line.append(&icon);
            line.append(&label);
            self.playing.append(&line);
        }
    }

    fn update_devices(self: &Rc<Self>, st: &Status) {
        let devices: Vec<&Sink> = st.devices().collect();
        let same = {
            let rows = self.rows.borrow();
            rows.len() == devices.len() && rows.iter().zip(&devices).all(|(r, d)| r.name == d.name)
        };
        if !same {
            for r in self.rows.borrow_mut().drain(..) {
                self.group.remove(&r.row);
            }
            let rows: Vec<DeviceRow> = devices.iter().map(|d| self.device_row(d)).collect();
            for r in &rows {
                self.group.add(&r.row);
            }
            self.rows.replace(rows);
        }
        self.empty.set_visible(devices.is_empty());
        self.footer.set_visible(!devices.is_empty());
        self.selected.replace(st.config.selected.clone());
        let in_group: Vec<&str> = st
            .members_of(PRIMARY_SINK)
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        for (r, d) in self.rows.borrow().iter().zip(&devices) {
            r.row.set_title(&glib::markup_escape_text(&d.label));
            let mut parts: Vec<&str> = Vec::new();
            if d.is_default {
                parts.push("Default output");
            }
            if in_group.contains(&d.name.as_str()) {
                parts.push("Playing in the group");
            } else if d.is_running() {
                parts.push("Playing");
            } else if d.state == "SUSPENDED" {
                parts.push("Suspended");
            } else if d.state == "IDLE" {
                parts.push("Idle");
            }
            r.row.set_subtitle(&parts.join(" · "));
            r.make_default.set_visible(!d.is_default);
            r.star.set_visible(d.is_default);
            r.vol.update(d, st.config.max_volume);
            if !self.touched.get() {
                self.syncing.set(true);
                r.check.set_active(st.config.selected.contains(&d.name));
                self.syncing.set(false);
            }
        }
        self.refresh_apply();
    }

    fn device_row(self: &Rc<Self>, d: &Sink) -> DeviceRow {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&d.label))
            .tooltip_text(&d.name)
            .build();
        row.set_title_lines(1);
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Include in simultaneous output")
            .build();
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        row.add_suffix(&badge(d));
        let vol = VolumeControl::new(&self.ctx, 140);
        row.add_suffix(&vol.root);
        let make_default = gtk::Button::builder()
            .icon_name("non-starred-symbolic")
            .tooltip_text("Make default output")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.set_tooltip_text(Some("Default output"));
        star.add_css_class("accent");
        star.set_valign(gtk::Align::Center);
        star.set_margin_start(9);
        star.set_margin_end(9);
        star.set_visible(false);
        row.add_suffix(&make_default);
        row.add_suffix(&star);
        let (p, name) = (self.clone(), d.name.clone());
        make_default.connect_clicked(move |_| {
            let name = name.clone();
            p.apply_default("Could not change the default output", async move {
                engine::use_output(&name).await
            });
        });
        let p = self.clone();
        check.connect_toggled(move |_| {
            // A user toggle keeps the ticks until applied; snapshots stop overwriting them.
            if !p.syncing.get() {
                p.touched.set(true);
            }
            p.refresh_apply();
            p.refresh_simultaneous_label();
        });
        DeviceRow {
            name: d.name.clone(),
            row,
            check,
            vol,
            make_default,
            star,
        }
    }

    fn refresh_apply(&self) {
        let checked = self.checked();
        let selected = self.selected.borrow();
        let dirty =
            checked.len() != selected.len() || checked.iter().any(|c| !selected.contains(c));
        drop(selected);
        if !dirty {
            self.touched.set(false);
        }
        for r in self.rows.borrow().iter() {
            if r.check.is_active() {
                r.row.add_css_class("audio-active-row");
            } else {
                r.row.remove_css_class("audio-active-row");
            }
        }
        let active = self.active.get();
        let label = match (active, dirty) {
            (true, true) => "Apply selection •",
            (true, false) => "Apply selection",
            (false, _) => "Turn on with selection",
        };
        self.apply.set_label(label);
        let suggested = checked.len() >= 2 && (dirty || !active);
        if suggested {
            self.apply.add_css_class("suggested-action");
        } else {
            self.apply.remove_css_class("suggested-action");
        }
        self.apply
            .set_sensitive(checked.len() >= 2 && (dirty || !active));
    }

    fn update_sync(self: &Rc<Self>, st: &Status) {
        let members: Vec<&Sink> = if st.active {
            st.members_of(PRIMARY_SINK)
                .into_iter()
                .filter(|s| s.port.is_some())
                .collect()
        } else {
            Vec::new()
        };
        self.sync.set_visible(!members.is_empty());
        let same = {
            let rows = self.sync_rows.borrow();
            rows.len() == members.len() && rows.iter().zip(&members).all(|(r, m)| r.name == m.name)
        };
        if !same {
            for r in self.sync_rows.borrow_mut().drain(..) {
                self.sync.remove(&r.row);
            }
            let rows: Vec<SyncRow> = members.iter().map(|m| self.sync_row(m)).collect();
            for r in &rows {
                self.sync.add(&r.row);
            }
            self.sync_rows.replace(rows);
        }
        for (r, m) in self.sync_rows.borrow().iter().zip(&members) {
            if r.last_user
                .get()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
            {
                continue;
            }
            r.updating.set(true);
            r.row
                .set_value(m.latency_offset_us.unwrap_or(0) as f64 / 1000.0);
            r.updating.set(false);
        }
    }

    fn sync_row(self: &Rc<Self>, m: &Sink) -> SyncRow {
        let row = adw::SpinRow::with_range(-500.0, 2000.0, 5.0);
        row.set_title(&glib::markup_escape_text(&m.label));
        row.set_subtitle("Latency offset (ms)");
        row.set_digits(0);
        let updating = Rc::new(Cell::new(false));
        let last_user = Rc::new(Cell::new(None));
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        let (p, name, upd, lu) = (
            self.clone(),
            m.name.clone(),
            updating.clone(),
            last_user.clone(),
        );
        row.connect_value_notify(move |r| {
            if upd.get() {
                return;
            }
            lu.set(Some(Instant::now()));
            if let Some(id) = pending.take() {
                id.remove();
            }
            let (ctx, name, usec, pending2) = (
                p.ctx.clone(),
                name.clone(),
                (r.value() * 1000.0).round() as i64,
                pending.clone(),
            );
            let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
                pending2.take();
                act(&ctx, "Could not set the latency offset", async move {
                    engine::set_latency_offset(&name, usec)
                        .await
                        .map(|()| String::new())
                });
            });
            pending.replace(Some(id));
        });
        SyncRow {
            name: m.name.clone(),
            row,
            updating,
            last_user,
        }
    }
}

/// Explains the selector's current state and what picking an entry does.
fn default_subtitle(st: &Status) -> String {
    match st.current_choice() {
        None if st.default_sink.is_empty() => {
            "No default output is set. Pick where system sound goes.".to_owned()
        }
        None => format!(
            "Sound currently goes to {}, which is not listed here. Pick where system sound goes.",
            st.label(&st.default_sink)
        ),
        Some(OutputChoice::Simultaneous) => "Plays on every output in the group. Picking a single output turns simultaneous output off; the ticks below are kept.".to_owned(),
        Some(OutputChoice::Device(_)) if st.active => "Simultaneous output still plays apps moved onto it, but is not the default. Picking an output turns it off; the ticks below are kept.".to_owned(),
        Some(OutputChoice::Device(_)) => "Where apps play unless a routing rule sends them elsewhere. Simultaneous output plays on every output ticked below.".to_owned(),
    }
}
