//! Pairing agent bridged to the GTK UI.
//!
//! The agent is registered only for the duration of a pairing started from the
//! Bluetooth page ([`crate::bt::pair`]), never as the default agent: BlueZ sends the
//! requests of a `Pair()` call to the agent of the calling connection, while
//! the desktop shell or another tool may already be the default agent for
//! incoming requests. Keeping it pairing-scoped means we never fight over who answers.
//!
//! Callbacks run on tokio; each one posts a [`Request`] (carrying a oneshot reply)
//! to [`requests`], which the page drains on the GTK main thread to show dialogs.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

use async_channel::{Receiver, Sender};
use bluer::agent::{
    Agent, AuthorizeService, DisplayPasskey, DisplayPinCode, ReqError, ReqResult,
    RequestAuthorization, RequestConfirmation, RequestPasskey, RequestPinCode,
};
use bluer::{Address, UuidExt};
use hyprdeck_core::events::{self, AppEvent};
use tokio::sync::oneshot;

use crate::{PAGE_ID, bt, info};

pub type Reply<T> = oneshot::Sender<ReqResult<T>>;

pub enum Request {
    PinCode {
        device: String,
        reply: Reply<String>,
    },
    Passkey {
        device: String,
        reply: Reply<u32>,
    },
    /// Show a code to type on the remote device; `done` resolves (Ok or Err) when it
    /// should no longer be displayed.
    Display {
        address: Address,
        device: String,
        code: String,
        entered: Option<u16>,
        done: oneshot::Receiver<()>,
    },
    Confirm {
        device: String,
        passkey: u32,
        reply: Reply<()>,
    },
    Authorize {
        device: String,
        reply: Reply<()>,
    },
    AuthorizeService {
        device: String,
        service: String,
        reply: Reply<()>,
    },
}

static CHANNEL: LazyLock<(Sender<Request>, Receiver<Request>)> =
    LazyLock::new(async_channel::unbounded);
/// Whether the Bluetooth page is currently on screen (set by the page on map/unmap).
static UI_VISIBLE: AtomicBool = AtomicBool::new(false);

pub fn requests() -> Receiver<Request> {
    CHANNEL.1.clone()
}

pub fn set_ui_visible(visible: bool) {
    UI_VISIBLE.store(visible, Ordering::Relaxed);
}

fn post(req: Request) {
    if !UI_VISIBLE.load(Ordering::Relaxed) {
        // The page builds lazily and starts draining requests when shown.
        events::send(AppEvent::ShowPage(PAGE_ID.into()));
    }
    let _ = CHANNEL.0.try_send(req);
}

/// Post a request and wait for the user's answer. If BlueZ cancels the request,
/// bluer drops this future, dropping the receiver, which closes the dialog.
async fn ask<T>(make: impl FnOnce(Reply<T>) -> Request) -> ReqResult<T> {
    let (tx, rx) = oneshot::channel();
    post(make(tx));
    rx.await.unwrap_or(Err(ReqError::Canceled))
}

/// Build an agent with all interactive callbacks (capability KeyboardDisplay).
pub fn agent() -> Agent {
    Agent {
        request_default: false,
        request_pin_code: Some(Box::new(|r: RequestPinCode| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                ask(|reply| Request::PinCode { device, reply }).await
            })
        })),
        display_pin_code: Some(Box::new(|r: DisplayPinCode| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                post(Request::Display {
                    address: r.device,
                    device,
                    code: r.pincode,
                    entered: None,
                    done: r.cancel,
                });
                Ok(())
            })
        })),
        request_passkey: Some(Box::new(|r: RequestPasskey| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                ask(|reply| Request::Passkey { device, reply }).await
            })
        })),
        display_passkey: Some(Box::new(|r: DisplayPasskey| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                post(Request::Display {
                    address: r.device,
                    device,
                    code: format!("{:06}", r.passkey),
                    entered: Some(r.entered),
                    done: r.cancel,
                });
                Ok(())
            })
        })),
        request_confirmation: Some(Box::new(|r: RequestConfirmation| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                ask(|reply| Request::Confirm {
                    device,
                    passkey: r.passkey,
                    reply,
                })
                .await
            })
        })),
        request_authorization: Some(Box::new(|r: RequestAuthorization| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                ask(|reply| Request::Authorize { device, reply }).await
            })
        })),
        authorize_service: Some(Box::new(|r: AuthorizeService| {
            Box::pin(async move {
                let device = bt::device_label(r.device).await;
                let service = info::service_name(r.service.as_u16())
                    .map_or_else(|| r.service.to_string(), str::to_owned);
                ask(|reply| Request::AuthorizeService {
                    device,
                    service,
                    reply,
                })
                .await
            })
        })),
        ..Default::default()
    }
}
