//! UI plumbing shared by all pages: the page registry type and [`Ctx`], the
//! handle pages use for toasts, dialogs and async work.
//!
//! # Pango markup
//!
//! libadwaita parses several texts as Pango markup by default, so dynamic text
//! containing `&`, `<` or `>` (commands, paths, device names, errors) logs
//! "Failed to set text … from markup" and renders blank. Rules:
//!
//! - Rows (`ActionRow`, `ExpanderRow`, `SwitchRow`, `ComboRow`, `SpinRow`,
//!   `EntryRow`, `ButtonRow`) showing dynamic text: build with
//!   `.use_markup(false)` and set the texts *after* `build()` with
//!   `set_title`/`set_subtitle`. Passing them to the builder does not work:
//!   GObject holds back `notify::use-markup` until construction finishes, so
//!   the labels still parse the builder's title/subtitle as markup.
//!   (Rows that keep markup on may instead escape with
//!   [`glib::markup_escape_text`], including in later setters.)
//! - `PreferencesGroup` title/description and `StatusPage` description have no
//!   switch: pass dynamic text through [`glib::markup_escape_text`].
//! - Toasts: use [`plain_toast`] (or [`Ctx::toast`]).
//! - Banners: build with `.use_markup(false)` (the default differs between
//!   libadwaita versions). `AlertDialog` heading/body are plain by default.

use std::future::Future;

use adw::prelude::*;
use gtk::glib;

/// A sidebar entry. `build` is called lazily the first time the page is opened.
#[derive(Clone, Copy)]
pub struct PageInfo {
    /// Stable id used for deep links (`hyprdeck --page display`).
    pub id: &'static str,
    pub title: &'static str,
    /// Symbolic icon name.
    pub icon: &'static str,
    pub build: fn(&Ctx) -> gtk::Widget,
}

#[derive(Clone)]
pub struct Ctx {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
}

impl Ctx {
    pub fn toast(&self, msg: impl AsRef<str>) {
        let toast = plain_toast(msg.as_ref());
        toast.set_timeout(4);
        self.toasts.add_toast(toast);
    }

    /// Log and show an error toast: "`what`: `err`".
    pub fn error(&self, what: &str, err: &anyhow::Error) {
        tracing::error!("{what}: {err:#}");
        let toast = plain_toast(&format!("{what}: {err:#}"));
        toast.set_timeout(8);
        self.toasts.add_toast(toast);
    }

    /// Run a future on the GTK main loop (may hold widgets).
    pub fn spawn(&self, fut: impl Future<Output = ()> + 'static) {
        glib::spawn_future_local(fut);
    }

    /// Ask for confirmation; resolves `true` when the accept button is chosen.
    pub async fn confirm(
        &self,
        heading: &str,
        body: &str,
        accept: &str,
        destructive: bool,
    ) -> bool {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("accept", accept)]);
        dialog.set_response_appearance(
            "accept",
            if destructive {
                adw::ResponseAppearance::Destructive
            } else {
                adw::ResponseAppearance::Suggested
            },
        );
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.choose_future(Some(&self.window)).await == "accept"
    }
}

/// A toast whose title is shown literally (toast titles are markup by default).
pub fn plain_toast(title: &str) -> adw::Toast {
    adw::Toast::builder().use_markup(false).title(title).build()
}

static WINDOW_VISIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the main window is currently shown (readable from any thread), so
/// background work can avoid disrupting the user, e.g. delaying a restart.
pub fn window_visible() -> bool {
    WINDOW_VISIBLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Set by the app shell whenever the main window is shown or hidden.
pub fn set_window_visible(visible: bool) {
    WINDOW_VISIBLE.store(visible, std::sync::atomic::Ordering::Relaxed);
}

/// Call `f` every time `widget` becomes visible (page shown / window re-opened).
pub fn on_shown(widget: &impl IsA<gtk::Widget>, f: impl Fn() + 'static) {
    widget.connect_map(move |_| f());
}

/// Standard scrollable page body: a clamped vertical box inside a scrolled window.
/// Returns `(outer, content)`; append preference groups to `content`.
pub fn page_scaffold() -> (gtk::ScrolledWindow, gtk::Box) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let clamp = adw::Clamp::builder()
        .maximum_size(900)
        .tightening_threshold(600)
        .child(&content)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    (scroller, content)
}

/// Remove all children of a box/list container.
pub fn clear(container: &impl IsA<gtk::Widget>) {
    let w = container.as_ref();
    while let Some(child) = w.first_child() {
        child.unparent();
    }
}

/// Development harness: run a minimal window showing `pages` in a stack
/// switcher. Used by each feature crate's `examples/preview.rs` so pages can be
/// exercised without building the full app.
pub fn preview(app_id: &str, pages: Vec<PageInfo>) -> glib::ExitCode {
    let app = adw::Application::builder().application_id(app_id).build();
    app.connect_activate(move |app| {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(1000)
            .default_height(800)
            .build();
        let toasts = adw::ToastOverlay::new();
        let ctx = Ctx {
            window: window.clone(),
            toasts: toasts.clone(),
        };
        let stack = adw::ViewStack::new();
        for p in &pages {
            stack.add_titled_with_icon(&(p.build)(&ctx), Some(p.id), p.title, p.icon);
        }
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &adw::ViewSwitcher::builder()
                .stack(&stack)
                .policy(adw::ViewSwitcherPolicy::Wide)
                .build(),
        ));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&stack));
        toasts.set_child(Some(&view));
        window.set_content(Some(&toasts));
        window.set_title(Some("hyprdeck preview"));
        window.present();
    });
    app.run_with_args::<&str>(&[])
}
