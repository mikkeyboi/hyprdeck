//! App-wide events delivered to the GTK main loop, from any thread.

use std::sync::LazyLock;

use async_channel::{Receiver, Sender};

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// Present the main window.
    ShowWindow,
    /// Present the main window on a page (see [`crate::page::PageInfo::id`]).
    ShowPage(String),
    /// Show an in-app toast (if the window exists).
    Toast(String),
    /// Exit the whole application (tray + background services).
    Quit,
}

static CHANNEL: LazyLock<(Sender<AppEvent>, Receiver<AppEvent>)> =
    LazyLock::new(async_channel::unbounded);

/// Send an event to the main loop. Never blocks.
pub fn send(event: AppEvent) {
    let _ = CHANNEL.0.try_send(event);
}

/// The single consumer (the app shell).
pub fn receiver() -> Receiver<AppEvent> {
    CHANNEL.1.clone()
}
