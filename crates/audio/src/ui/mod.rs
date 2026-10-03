//! The Audio page: Outputs / Volumes / App Routing / Settings tabs.

mod outputs;
mod routing;
mod rule_dialog;
mod settings;
mod volumes;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::rt;
use hyprdeck_core::ui::Ctx;

use crate::engine::{self, Status};
use crate::pw::Sink;
use crate::{PAGE_ID, daemon};

const CSS: &str = "
.audio-hero { background-color: alpha(@accent_bg_color, 0.11); border: 1px solid alpha(@accent_color, 0.22);
  border-radius: 12px; padding: 14px 16px; }
.audio-hero.off { background-color: alpha(currentColor, 0.04); border-color: alpha(currentColor, 0.10); }
.audio-hero .hero-icon { color: @accent_color; }
.audio-hero.off .hero-icon { color: alpha(currentColor, 0.55); }
.audio-badge { background-color: alpha(currentColor, 0.08); border-radius: 999px; padding: 1px 8px; font-size: 0.8em; font-weight: bold; }
.audio-badge.bluetooth { color: @blue_3; }
.audio-badge.usb { color: @green_4; }
.audio-badge.hdmi { color: @purple_3; }
.audio-badge.analog { color: @orange_4; }
.audio-badge.virtual { color: @accent_color; }
.audio-active-row { box-shadow: inset 3px 0 0 @accent_color; }
.audio-live { color: @success_color; }
";

fn load_css() {
    thread_local!(static LOADED: Cell<bool> = const { Cell::new(false) });
    if LOADED.replace(true) {
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
    }
}

/// Tab of the Audio page holding the playing streams and routing rules.
pub const ROUTING_TAB: &str = "routing";

/// Tab to select once the Audio page exists; written from any thread.
static REQUESTED_TAB: Mutex<Option<&'static str>> = Mutex::new(None);

thread_local! {
    /// Tab stack of the built Audio page (GTK main thread only).
    static TABS: glib::WeakRef<adw::ViewStack> = glib::WeakRef::new();
}

/// Present the Audio page on `tab`. Callable from any thread (tray included):
/// a built page switches immediately, an unbuilt one opens on `tab`.
pub fn show_tab(tab: &'static str) {
    *REQUESTED_TAB.lock().unwrap_or_else(|e| e.into_inner()) = Some(tab);
    glib::MainContext::default().invoke(select_requested_tab);
    events::send(AppEvent::ShowPage(PAGE_ID.into()));
}

/// Apply a pending [`show_tab`] request to the live page (no-op until it is built).
fn select_requested_tab() {
    let Some(stack) = TABS.with(glib::WeakRef::upgrade) else {
        return;
    };
    if let Some(tab) = REQUESTED_TAB
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        stack.set_visible_child_name(tab);
    }
}

pub fn audio_page(ctx: &Ctx) -> gtk::Widget {
    load_css();
    daemon::start();
    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(
        &outputs::build(ctx),
        Some("outputs"),
        "Outputs",
        "audio-speakers-symbolic",
    );
    stack.add_titled_with_icon(
        &volumes::build(ctx),
        Some("volumes"),
        "Volumes",
        "audio-volume-high-symbolic",
    );
    stack.add_titled_with_icon(
        &routing::build(ctx),
        Some(ROUTING_TAB),
        "App Routing",
        "media-playlist-shuffle-symbolic",
    );
    stack.add_titled_with_icon(
        &settings::build(ctx),
        Some("settings"),
        "Settings",
        "emblem-system-symbolic",
    );
    let switcher = adw::InlineViewSwitcher::builder()
        .stack(&stack)
        .halign(gtk::Align::Center)
        .build();
    switcher.set_margin_top(12);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&switcher);
    root.append(&stack);
    TABS.with(|t| t.set(Some(&stack)));
    select_requested_tab();
    hyprdeck_core::ui::on_shown(&root, || daemon::request_refresh(false));
    root.upcast()
}

/// Call `f` with every published snapshot for as long as `anchor` is alive.
fn follow(anchor: &impl IsA<gtk::Widget>, f: impl Fn(&Result<Status, String>) + 'static) {
    let weak = anchor.as_ref().downgrade();
    glib::spawn_future_local(async move {
        let mut rx = daemon::subscribe();
        loop {
            let snap = rx.borrow_and_update().clone();
            if weak.upgrade().is_none() {
                break;
            }
            if let Some(s) = snap {
                f(&s);
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    });
}

/// Run an engine action; toast its message or surface the error.
fn act<F>(ctx: &Ctx, what: &'static str, fut: F)
where
    F: Future<Output = anyhow::Result<String>> + Send + 'static,
{
    act_then(ctx, what, fut, |_| {});
}

/// Like [`act`], then call `done(ok)` on the main loop (restore sensitivity etc).
fn act_then<F>(ctx: &Ctx, what: &'static str, fut: F, done: impl FnOnce(bool) + 'static)
where
    F: Future<Output = anyhow::Result<String>> + Send + 'static,
{
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        let r = daemon::ui_action(fut).await;
        match &r {
            Ok(msg) if !msg.is_empty() => ctx.toast(msg),
            Ok(_) => {}
            Err(e) => ctx.error(what, e),
        }
        done(r.is_ok());
    });
}

fn badge(sink: &Sink) -> gtk::Label {
    let l = gtk::Label::new(Some(sink.conn.label()));
    l.add_css_class("audio-badge");
    l.add_css_class(&sink.conn.label().to_lowercase());
    l.set_valign(gtk::Align::Center);
    l
}

/// Scale + percentage + mute toggle for one sink. Debounces drags and ignores
/// snapshots while it is being adjusted.
#[derive(Clone)]
struct VolumeControl {
    root: gtk::Box,
    scale: gtk::Scale,
    pct: gtk::Label,
    mute: gtk::ToggleButton,
    inner: Rc<VolumeInner>,
}

struct VolumeInner {
    sink: RefCell<String>,
    updating: Cell<bool>,
    pending: RefCell<Option<glib::SourceId>>,
    last_user: Cell<Option<Instant>>,
}

impl VolumeControl {
    fn new(ctx: &Ctx, width: i32) -> Self {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 150.0, 1.0);
        scale.set_draw_value(false);
        scale.set_width_request(width);
        scale.set_valign(gtk::Align::Center);
        scale.add_mark(100.0, gtk::PositionType::Bottom, None);
        let pct = gtk::Label::new(None);
        pct.set_width_chars(5);
        pct.set_xalign(1.0);
        pct.add_css_class("numeric");
        pct.add_css_class("dim-label");
        let mute = gtk::ToggleButton::builder()
            .icon_name("audio-volume-high-symbolic")
            .tooltip_text("Mute")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        root.append(&scale);
        root.append(&pct);
        root.append(&mute);
        let inner = Rc::new(VolumeInner {
            sink: RefCell::default(),
            updating: Cell::new(false),
            pending: RefCell::default(),
            last_user: Cell::new(None),
        });
        let vc = VolumeControl {
            root,
            scale,
            pct,
            mute,
            inner,
        };

        let (ctx1, this) = (ctx.clone(), vc.clone());
        vc.scale.connect_value_changed(move |s| {
            if this.inner.updating.get() {
                return;
            }
            let value = s.value().round();
            this.pct.set_label(&format!("{value}%"));
            this.inner.last_user.set(Some(Instant::now()));
            if let Some(id) = this.inner.pending.take() {
                id.remove();
            }
            let (ctx, inner) = (ctx1.clone(), this.inner.clone());
            let id = glib::timeout_add_local_once(Duration::from_millis(140), move || {
                inner.pending.take();
                let sink = inner.sink.borrow().clone();
                glib::spawn_future_local(async move {
                    if let Err(e) =
                        rt::run(async move { engine::set_volume(&sink, value).await }).await
                    {
                        ctx.error("Could not set volume", &e);
                    }
                });
            });
            this.inner.pending.replace(Some(id));
        });
        let (ctx2, this) = (ctx.clone(), vc.clone());
        vc.mute.connect_toggled(move |b| {
            this.set_mute_icon(b.is_active());
            if this.inner.updating.get() {
                return;
            }
            this.inner.last_user.set(Some(Instant::now()));
            let (sink, muted, ctx) = (
                this.inner.sink.borrow().clone(),
                b.is_active(),
                ctx2.clone(),
            );
            glib::spawn_future_local(async move {
                if let Err(e) = rt::run(async move { engine::set_mute(&sink, muted).await }).await {
                    ctx.error("Could not change mute", &e);
                }
            });
        });
        vc
    }

    fn set_mute_icon(&self, muted: bool) {
        self.mute.set_icon_name(if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        self.mute
            .set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
    }

    fn update(&self, sink: &Sink, max: u32) {
        self.inner.sink.replace(sink.name.clone());
        let busy = self.inner.pending.borrow().is_some()
            || self
                .inner
                .last_user
                .get()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(800));
        if busy {
            return;
        }
        self.inner.updating.set(true);
        self.scale.set_range(0.0, max as f64);
        self.scale.set_value(sink.volume as f64);
        self.pct.set_label(&format!("{}%", sink.volume));
        self.mute.set_active(sink.muted);
        self.set_mute_icon(sink.muted);
        self.inner.updating.set(false);
    }
}
