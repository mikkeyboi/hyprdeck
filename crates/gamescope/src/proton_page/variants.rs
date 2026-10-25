//! Lineage of Hyprdeck builds and variants, and the Create variant dialog.
use super::{Inventory, Page, ToolEntry, action_row};
use crate::proton::{self, InstalledTool, Target};
use adw::prelude::*;
use gtk::glib;
use hd_protonbuild::{
    builder as protonbuild,
    profile::{Family, GameProfile, SETTINGS},
    recipe::Recipe,
    variant::{self, Receipt},
};
use hyprdeck_core::{rt, ui};
use std::{path::Path, rc::Rc};

/// Where an installed tool came from, read from the receipts Hyprdeck writes into it.
pub(super) enum Origin {
    Upstream,
    /// A Hyprdeck build, made locally by the Build tab or downloaded as Hyprdeck Proton.
    Build(Recipe),
    Variant(Receipt),
}

pub(super) fn origin(tool: &Path) -> Origin {
    if tool.join(variant::RECEIPT).is_file()
        && let Ok(receipt) = variant::receipt(tool)
    {
        return Origin::Variant(receipt);
    }
    match protonbuild::installed_recipe(tool) {
        Some(recipe) => Origin::Build(recipe),
        None => Origin::Upstream,
    }
}

impl Origin {
    pub(super) fn label(&self, managed: bool) -> String {
        match self {
            Self::Variant(receipt) => format!("Hyprdeck variant of {}", source_name(receipt)),
            Self::Build(recipe) => format!(
                "Hyprdeck build of {} · {}",
                recipe.base,
                recipe.flags.label()
            ),
            Self::Upstream if managed => "Hyprdeck-managed".into(),
            Self::Upstream => "Externally managed".into(),
        }
    }

    /// Hyprdeck can delete it even without a download receipt: it wrote the folder itself.
    pub(super) fn removable(&self) -> bool {
        !matches!(self, Self::Upstream)
    }
}

/// Removes a tool by whichever receipt proves Hyprdeck owns it.
pub(super) fn remove(target: &Target, tool: &InstalledTool) -> anyhow::Result<()> {
    if tool.managed {
        return proton::remove(target, tool);
    }
    match origin(&tool.path) {
        Origin::Variant(_) => {
            proton::require_stopped(target)?;
            variant::remove(&tool.path)
        }
        Origin::Build(_) => {
            proton::require_stopped(target)?;
            protonbuild::uninstall(&tool.path)
        }
        Origin::Upstream => proton::remove(target, tool),
    }
}

fn source_name(receipt: &Receipt) -> String {
    if receipt.source_name.is_empty() {
        receipt
            .source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        receipt.source_name.clone()
    }
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Ge => "GE-Proton",
        Family::CachyOs => "Proton-CachyOS",
        Family::Valve => "Valve Proton",
    }
}

fn recipe_line(recipe: &Recipe) -> String {
    let mut line = format!(
        "GE-Proton source {} → {} compiler flags",
        recipe.base,
        recipe.flags.label()
    );
    for pin in &recipe.pins {
        line.push_str(&format!(
            "\nPinned {}={} ({})",
            pin.variable, pin.value, pin.reason
        ));
    }
    line
}

/// "HDR, Upgrade DLSS · 3 games override them", for the settings a variant applies.
pub(super) fn customizations(data: &Inventory, family: Family) -> String {
    let global = data.profiles.global.enabled_titles();
    let mut text = if global.is_empty() {
        "Global defaults: all off".to_owned()
    } else {
        format!("Global defaults: {}", global.join(", "))
    };
    match data.profiles.games.len() {
        0 => {}
        1 => text.push_str(" · 1 game overrides them"),
        n => text.push_str(&format!(" · {n} games override them")),
    }
    let unsupported = unsupported_titles(&data.profiles.global, family);
    if !unsupported.is_empty() {
        text.push_str(&format!(
            "\nNot supported by {}: {}",
            family_name(family),
            unsupported.join(", ")
        ));
    }
    text
}

fn unsupported_titles(profile: &GameProfile, family: Family) -> Vec<&'static str> {
    profile
        .resolve(family)
        .unsupported
        .iter()
        .filter_map(|key| SETTINGS.iter().find(|s| s.key == *key).map(|s| s.title))
        .collect()
}

fn created(seconds: u64) -> String {
    glib::DateTime::from_unix_local(seconds as i64)
        .and_then(|time| time.format("%Y-%m-%d %H:%M"))
        .map(|text| text.to_string())
        .unwrap_or_else(|_| "unknown".into())
}

enum Event {
    Progress(u64, u64),
    Done(Result<std::path::PathBuf, String>),
}

impl Page {
    /// Lineage and customization rows inside an installed tool's expander.
    pub(super) fn lineage_rows(&self, row: &adw::ExpanderRow, entry: &ToolEntry, data: &Inventory) {
        match &entry.origin {
            Origin::Variant(receipt) => {
                let mut lineage = format!("{} → {}", source_name(receipt), receipt.display_name);
                if let Some(recipe) = &receipt.build {
                    lineage = format!("{}\n{lineage}", recipe_line(recipe));
                }
                if !receipt.source.exists() {
                    lineage.push_str(&format!(
                        "\nThe original at {} was removed; this variant keeps working because its files are hard links.",
                        receipt.source.display()
                    ));
                }
                row.add_row(&action_row("Lineage", &lineage));
                row.add_row(&action_row(
                    "Customizations",
                    &format!(
                        "{}\nRead at every launch; change them under Gaming > Games.",
                        customizations(data, receipt.family)
                    ),
                ));
                if receipt.created > 0 {
                    row.add_row(&action_row("Created", &created(receipt.created)));
                }
            }
            Origin::Build(recipe) => {
                row.add_row(&action_row("Lineage", &recipe_line(recipe)));
            }
            Origin::Upstream => {}
        }
        let children: Vec<_> = data
            .tools
            .iter()
            .filter_map(|other| match &other.origin {
                Origin::Variant(receipt) if receipt.source == entry.tool.path => {
                    Some(other.tool.display_name.as_str())
                }
                _ => None,
            })
            .collect();
        if !children.is_empty() {
            row.add_row(&action_row("Hyprdeck variants", &children.join("\n")));
        }
    }

    /// "Create Hyprdeck variant…" on a runner, or "Update variant…" on a variant.
    pub(super) fn variant_button(
        self: &Rc<Self>,
        row: &adw::ExpanderRow,
        target: &Target,
        entry: &ToolEntry,
        data: &Inventory,
    ) {
        let source = match &entry.origin {
            Origin::Variant(receipt) => data
                .tools
                .iter()
                .find(|other| other.tool.path == receipt.source)
                .map(|other| other.tool.clone()),
            _ => Some(entry.tool.clone()),
        };
        let button = gtk::Button::with_label(match entry.origin {
            Origin::Variant(_) => "Update variant…",
            _ => "Create Hyprdeck variant…",
        });
        button.set_valign(gtk::Align::Center);
        match &source {
            Some(_) if matches!(entry.origin, Origin::Variant(_)) => button.set_tooltip_text(Some(
                "Relink this variant to its runner, for example after the runner was updated",
            )),
            Some(_) => button.set_tooltip_text(Some(
                "A linked copy of this runner that applies Hyprdeck's game settings at launch",
            )),
            None => {
                button.set_sensitive(false);
                button.set_tooltip_text(Some("The original runner is no longer installed"));
            }
        }
        row.add_suffix(&button);
        button.connect_clicked({
            let weak = Rc::downgrade(self);
            let target = target.clone();
            move |_| {
                if let (Some(page), Some(source)) = (weak.upgrade(), source.clone()) {
                    page.variant_dialog(target.clone(), source);
                }
            }
        });
    }

    /// Shows what the variant will be made from and apply, then creates it with progress.
    fn variant_dialog(self: &Rc<Self>, target: Target, source: InstalledTool) {
        let inventory = self.inventory.borrow();
        let Some(data) = inventory.as_ref() else {
            return;
        };
        let Ok(folder) = variant::variant_name(&source.path) else {
            return;
        };
        let family = Family::detect(&folder[variant::PREFIX.len()..])
            .or_else(|| Family::detect(&source.display_name))
            .unwrap_or(Family::Ge);
        let destination = target.root.join(&folder);
        let dialog = adw::Dialog::builder()
            .title("Create Hyprdeck variant")
            .content_width(620)
            .content_height(640)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let (scroller, content) = ui::page_scaffold();
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));

        let lineage = adw::PreferencesGroup::builder()
            .title("Lineage")
            .description("A variant is a hard-linked copy of a runner with Hyprdeck's launcher added. It takes almost no extra disk space, and the original runner is never changed.")
            .build();
        lineage.add(&action_row(
            "Based on",
            &format!("{}\n{}", source.display_name, source.path.display()),
        ));
        if let Some(recipe) = protonbuild::installed_recipe(&source.path) {
            lineage.add(&action_row("Which was built from", &recipe_line(&recipe)));
        }
        lineage.add(&action_row(
            "Runner family",
            &format!(
                "{}: Hyprdeck translates each setting to this runner's own variables",
                family_name(family)
            ),
        ));
        lineage.add(&action_row(
            "Creates",
            &format!(
                "{} (Hyprdeck) in {}{}",
                source.display_name,
                folder,
                if destination.exists() {
                    "\nReplaces the existing variant; game assignments stay"
                } else {
                    ""
                }
            ),
        ));
        content.append(&lineage);

        let custom = adw::PreferencesGroup::builder()
            .title("Customizations")
            .description("The variant reads these at every launch, so changing them under Gaming > Games never needs a new variant. Steam launch options still win.")
            .build();
        custom.add(&action_row("Applies", &customizations(data, family)));
        content.append(&custom);
        drop(inventory);

        let progress = gtk::ProgressBar::builder()
            .show_text(true)
            .visible(false)
            .build();
        content.append(&progress);
        let status = gtk::Label::builder()
            .wrap(true)
            .xalign(0.)
            .selectable(true)
            .build();
        content.append(&status);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let create = gtk::Button::with_label("Create variant");
        create.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&create);
        content.append(&buttons);
        cancel.connect_clicked({
            let dialog = dialog.downgrade();
            move |_| {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
            }
        });
        create.connect_clicked({
            let weak = Rc::downgrade(self);
            let dialog = dialog.downgrade();
            move |create| {
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if create.label().as_deref() == Some("Done") {
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                    return;
                }
                let profiles = page
                    .inventory
                    .borrow()
                    .as_ref()
                    .map(|data| data.profiles.clone())
                    .unwrap_or_default();
                create.set_sensitive(false);
                cancel.set_sensitive(false);
                progress.set_visible(true);
                progress.set_fraction(0.);
                progress.set_text(Some("Linking files…"));
                status.set_label("");
                let (tx, rx) = async_channel::bounded(16);
                let source_path = source.path.clone();
                let name = source.display_name.clone();
                let root = target.root.clone();
                rt::spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        let result =
                            variant::create(&source_path, &root, &name, &profiles, &mut |done, total| {
                                let _ = tx.try_send(Event::Progress(done, total));
                            });
                        let _ = tx.send_blocking(Event::Done(result.map_err(|e| format!("{e:#}"))));
                    })
                    .await;
                });
                let create = create.clone();
                let cancel = cancel.clone();
                let progress = progress.clone();
                let status = status.clone();
                let weak = Rc::downgrade(&page);
                glib::spawn_future_local(async move {
                    while let Ok(event) = rx.recv().await {
                        match event {
                            Event::Progress(done, total) => {
                                progress.set_fraction(done as f64 / total.max(1) as f64);
                                progress.set_text(Some(&format!("Linked {done} of {total} files")));
                            }
                            Event::Done(Ok(path)) => {
                                progress.set_fraction(1.);
                                progress.set_text(Some("Done"));
                                status.set_label(&format!(
                                    "Created {}. Restart Steam to see it, then pick it for a game under Gaming > Games.",
                                    path.display()
                                ));
                                create.set_label("Done");
                                create.set_sensitive(true);
                                if let Some(page) = weak.upgrade() {
                                    page.reload();
                                }
                                break;
                            }
                            Event::Done(Err(error)) => {
                                progress.set_visible(false);
                                status.set_label(&format!("Could not create the variant: {error}"));
                                create.set_sensitive(true);
                                cancel.set_sensitive(true);
                                break;
                            }
                        }
                    }
                });
            }
        });
        dialog.present(Some(&self.ctx.window));
    }
}
