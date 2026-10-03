//! Small shared widgets: keycaps, badges, CSS, debouncing.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::actions::key_label;

const CSS: &str = "
.hd-keycap {
  font-size: 0.85em;
  font-weight: 600;
  padding: 1px 7px;
  min-width: 14px;
  border-radius: 6px;
  border: 1px solid alpha(currentColor, 0.22);
  box-shadow: inset 0 -2px alpha(currentColor, 0.18);
  background: alpha(currentColor, 0.06);
}
.hd-badge {
  font-size: 0.8em;
  font-weight: 600;
  padding: 1px 8px;
  border-radius: 999px;
  background: alpha(currentColor, 0.08);
}
.hd-badge.accent { background: alpha(@accent_bg_color, 0.18); color: @accent_color; }
.hd-badge.warning { background: alpha(@warning_bg_color, 0.2); color: @warning_color; }
.hd-capture { font-weight: 600; }
";

/// Install the crate's CSS once per display (main thread).
pub fn ensure_css() {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if DONE.get() {
        return;
    }
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        DONE.set(true);
    }
}

/// Keycaps for a combo string (`SUPER + SHIFT + S`).
pub fn keycaps(combo: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    b.set_valign(gtk::Align::Center);
    for part in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let l = gtk::Label::new(Some(&key_label(part)));
        l.add_css_class("hd-keycap");
        if key_label(part) != part {
            l.set_tooltip_text(Some(part));
        }
        b.append(&l);
    }
    b
}

pub fn badge(text: &str, class: Option<&str>) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("hd-badge");
    if let Some(c) = class {
        l.add_css_class(c);
    }
    l.set_valign(gtk::Align::Center);
    l
}

/// Dim caption label showing where a value comes from.
pub fn origin_label() -> gtk::Label {
    let l = gtk::Label::new(None);
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l.set_valign(gtk::Align::Center);
    l.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    l.set_max_width_chars(24);
    l
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(tooltip));
    b.add_css_class("flat");
    b.set_valign(gtk::Align::Center);
    b
}

/// Coalesce rapid changes (spin buttons): only the last call within `ms` runs.
#[derive(Clone, Default)]
pub struct Debounce(Rc<Cell<u64>>);

impl Debounce {
    pub fn run(&self, ms: u64, f: impl FnOnce() + 'static) {
        let id = self.0.get() + 1;
        self.0.set(id);
        let latest = self.0.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            if latest.get() == id {
                f();
            }
        });
    }
}

/// Reentrancy guard for programmatic widget updates.
#[derive(Clone, Default)]
pub struct Guard(Rc<Cell<bool>>);

impl Guard {
    pub fn active(&self) -> bool {
        self.0.get()
    }

    pub fn hold(&self, f: impl FnOnce()) {
        let prev = self.0.replace(true);
        f();
        self.0.set(prev);
    }
}

/// A centered spinner placeholder.
pub fn loading() -> gtk::Widget {
    let s = adw::Spinner::new();
    s.set_size_request(32, 32);
    s.set_halign(gtk::Align::Center);
    s.set_margin_top(48);
    s.upcast()
}

/// Error placeholder with a retry button.
pub fn error_page(what: &str, err: &anyhow::Error, retry: impl Fn() + 'static) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title(what)
        .description(glib::markup_escape_text(&format!("{err:#}")))
        .build();
    let b = gtk::Button::with_label("Retry");
    b.add_css_class("pill");
    b.set_halign(gtk::Align::Center);
    b.connect_clicked(move |_| retry());
    page.set_child(Some(&b));
    page.upcast()
}

/// Surface config errors Hyprland reported after a reload.
pub fn report_errors(ctx: &hyprdeck_core::ui::Ctx, ok_msg: &str, errors: &[String]) {
    match errors.first() {
        None => ctx.toast(ok_msg),
        Some(first) => ctx.error(
            "Hyprland reported config errors",
            &anyhow::anyhow!(
                "{first}{}",
                if errors.len() > 1 {
                    format!(" (+{} more)", errors.len() - 1)
                } else {
                    String::new()
                }
            ),
        ),
    }
}
