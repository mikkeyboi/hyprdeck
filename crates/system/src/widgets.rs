//! Small widget helpers shared by both pages.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use hyprdeck_core::ui::Ctx;

pub fn button(label: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build()
}

/// `~/…` for paths inside the home directory.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(hyprdeck_core::store::home()) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

/// Show a spinner and make `button` insensitive while `on`; `label` is the
/// button's normal label.
pub fn busy(button: &gtk::Button, label: &str, on: bool) {
    button.set_sensitive(!on);
    if on {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        b.append(&adw::Spinner::new());
        b.append(&gtk::Label::new(Some(label)));
        button.set_child(Some(&b));
    } else {
        button.set_label(label);
    }
}

/// Action row showing plain (non-markup) text.
pub fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder().use_markup(false).build();
    r.set_title(title);
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    r
}

/// Read-only monospace text view.
pub fn mono_view() -> gtk::TextView {
    gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build()
}

pub fn copy_text(ctx: &Ctx, text: &str) {
    ctx.window.clipboard().set_text(text);
    ctx.toast("Copied to clipboard");
}

/// Open a file or folder with its default application.
pub fn open_path(ctx: &Ctx, path: PathBuf) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
    let ctx2 = ctx.clone();
    launcher.launch(Some(&ctx.window), gio::Cancellable::NONE, move |r| {
        if let Err(e) = r {
            ctx2.error(
                &format!("Opening {}", path.display()),
                &anyhow::anyhow!("{e}"),
            );
        }
    });
}

/// Dialog showing a text file with Copy and Open actions.
pub fn text_dialog(ctx: &Ctx, title: &str, text: &str, path: Option<PathBuf>) {
    let view = mono_view();
    view.buffer().set_text(text);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .build();
    let header = adw::HeaderBar::new();
    let copy = icon_button("edit-copy-symbolic", "Copy");
    let c = ctx.clone();
    let t = text.to_owned();
    copy.connect_clicked(move |_| copy_text(&c, &t));
    header.pack_end(&copy);
    if let Some(path) = path {
        let open = icon_button("document-open-symbolic", "Open in the default app");
        let c = ctx.clone();
        open.connect_clicked(move |_| open_path(&c, path.clone()));
        header.pack_end(&open);
    }
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(960)
        .content_height(720)
        .child(&tv)
        .build();
    dialog.present(Some(&ctx.window));
}

/// Call `f(value)` once the spin row has been still for a moment (avoids a
/// write per arrow click).
pub fn on_spin_settled(row: &adw::SpinRow, f: impl Fn(f64) + 'static) {
    let generation = Rc::new(Cell::new(0u32));
    let f = Rc::new(f);
    row.connect_value_notify(move |row| {
        let g = generation.get().wrapping_add(1);
        generation.set(g);
        let (generation, f, value) = (generation.clone(), f.clone(), row.value());
        glib::spawn_future_local(async move {
            glib::timeout_future(Duration::from_millis(700)).await;
            if generation.get() == g {
                f(value);
            }
        });
    });
}
