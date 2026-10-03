//! Create/edit a routing rule (match, target outputs, optional stream volume).

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::ui::Ctx;

use crate::config::{MatchKind, Route, rule_from_form};
use crate::engine::{self, Status};
use crate::pw::Stream;

/// `existing` edits a rule (keeping its id and enabled state); `from_stream` pre-fills a new one.
pub fn open(ctx: &Ctx, st: &Status, existing: Option<&Route>, from_stream: Option<&Stream>) {
    let template = match (existing, from_stream) {
        (Some(r), _) => r.clone(),
        (None, Some(s)) => {
            let (kind, value) = if !s.binary.is_empty() {
                (MatchKind::Binary, s.binary.clone())
            } else if !s.app_name.is_empty() {
                (MatchKind::App, s.app_name.clone())
            } else {
                (MatchKind::Media, s.media_name.clone())
            };
            Route {
                label: s.label(),
                match_kind: kind,
                match_value: value,
                ..Route::default()
            }
        }
        (None, None) => Route::default(),
    };

    let dialog = adw::Dialog::builder()
        .title(if existing.is_some() {
            "Edit routing rule"
        } else {
            "New routing rule"
        })
        .content_width(480)
        .content_height(640)
        .build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let banner = adw::Banner::builder()
        .use_markup(false)
        .revealed(false)
        .build();

    let page = adw::PreferencesPage::new();
    let rule = adw::PreferencesGroup::builder().title("Rule").build();
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(&template.label)
        .build();
    let kinds = gtk::StringList::new(&MatchKind::ALL.map(MatchKind::label));
    let kind = adw::ComboRow::builder()
        .title("Match on")
        .model(&kinds)
        .build();
    kind.set_selected(
        MatchKind::ALL
            .iter()
            .position(|k| *k == template.match_kind)
            .unwrap_or(0) as u32,
    );
    let value = adw::EntryRow::builder()
        .title("Matches when it contains")
        .text(&template.match_value)
        .build();
    rule.add(&name);
    rule.add(&kind);
    rule.add(&value);
    page.add(&rule);

    let targets = adw::PreferencesGroup::builder()
        .title("Send to")
        .description("One output moves the app there; several play it on all of them at once.")
        .build();
    let mut checks: Vec<(String, gtk::CheckButton)> = Vec::new();
    for d in st.devices() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&d.label))
            .subtitle(d.conn.label())
            .build();
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(template.sinks.contains(&d.name))
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(d.conn.icon()));
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        targets.add(&row);
        checks.push((d.name.clone(), check));
    }
    // Targets of an edited rule that are not connected right now stay selected.
    let offline: Vec<String> = template
        .sinks
        .iter()
        .filter(|n| st.sink(n).is_none())
        .cloned()
        .collect();
    for n in &offline {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(n))
            .subtitle("Not connected")
            .build();
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(true)
            .build();
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        targets.add(&row);
        checks.push((n.clone(), check));
    }
    if checks.is_empty() {
        targets.add(
            &adw::ActionRow::builder()
                .title("No outputs available")
                .subtitle("Connect a device, then reopen this dialog.")
                .build(),
        );
    }
    page.add(&targets);

    let extra = adw::PreferencesGroup::new();
    let volume_row = adw::ExpanderRow::builder()
        .title("Set stream volume")
        .subtitle("Applied each time a stream is moved by this rule")
        .show_enable_switch(true)
        .enable_expansion(template.volume.is_some())
        .build();
    let volume = adw::SpinRow::with_range(0.0, st.config.max_volume as f64, 5.0);
    volume.set_title("Volume (percent)");
    volume.set_value(template.volume.unwrap_or(100) as f64);
    volume_row.add_row(&volume);
    extra.add(&volume_row);
    page.add(&extra);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&banner);
    view.set_content(Some(&page));
    dialog.set_child(Some(&view));
    dialog.set_default_widget(Some(&save));

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let (d, ctx2, existing) = (dialog.clone(), ctx.clone(), existing.cloned());
    save.connect_clicked(move |btn| {
        let kind = MatchKind::ALL
            .get(kind.selected() as usize)
            .copied()
            .unwrap_or_default();
        let sinks: Vec<String> = checks
            .iter()
            .filter(|(_, c)| c.is_active())
            .map(|(n, _)| n.clone())
            .collect();
        let vol = volume_row
            .enables_expansion()
            .then(|| volume.value().round() as u32);
        match rule_from_form(
            existing.as_ref(),
            &name.text(),
            kind,
            &value.text(),
            sinks,
            vol,
        ) {
            Err(msg) => {
                banner.set_title(msg);
                banner.set_revealed(true);
            }
            Ok(route) => {
                btn.set_sensitive(false);
                let (d, btn) = (d.clone(), btn.clone());
                super::act_then(
                    &ctx2,
                    "Could not save the rule",
                    engine::save_route(route),
                    move |ok| {
                        btn.set_sensitive(true);
                        if ok {
                            d.close();
                        }
                    },
                );
            }
        }
    });
    dialog.present(Some(&ctx.window));
}
