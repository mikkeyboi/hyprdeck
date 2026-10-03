//! BlueZ access via `bluer`. Everything here runs on the shared tokio runtime.

use std::collections::HashSet;
use std::pin::{Pin, pin};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_channel::Sender;
use bluer::agent::Agent;
use bluer::{
    Adapter, AdapterEvent, AdapterProperty, Address, DeviceEvent, DeviceProperty, ErrorKind,
    InternalErrorKind, Session, SessionEvent,
};
use futures_util::stream::{SelectAll, Stream, StreamExt};
use tokio::sync::OnceCell;

use crate::info::{AdapterInfo, DeviceInfo, explain};

static SESSION: OnceCell<Session> = OnceCell::const_new();

/// Process-wide BlueZ session (one system-bus connection; agents registered on it
/// receive the pairing requests of `Pair()` calls made through it).
pub async fn session() -> Result<&'static Session> {
    SESSION
        .get_or_try_init(|| async { Session::new().await })
        .await
        .context("cannot connect to BlueZ on the system bus (is bluetooth.service running?)")
}

pub async fn adapter() -> Result<Adapter> {
    session()
        .await?
        .default_adapter()
        .await
        .map_err(|e| match e.kind {
            ErrorKind::NotFound => anyhow!("no Bluetooth adapter found"),
            _ if is_no_bluez(&e) => NoBluez.into(),
            _ => err(e),
        })
}

/// Turn a bluer error into a readable anyhow error.
pub fn err(e: bluer::Error) -> anyhow::Error {
    anyhow!(explain(&e.kind, &e.message))
}

pub async fn adapter_info(a: &Adapter) -> Result<AdapterInfo> {
    let (alias, address, powered, discoverable, discoverable_timeout, pairable, discovering) =
        tokio::try_join!(
            a.alias(),
            a.address(),
            a.is_powered(),
            a.is_discoverable(),
            a.discoverable_timeout(),
            a.is_pairable(),
            a.is_discovering(),
        )
        .map_err(err)?;
    Ok(AdapterInfo {
        name: a.name().to_owned(),
        alias,
        address,
        powered,
        discoverable,
        discoverable_timeout,
        pairable,
        discovering,
    })
}

pub async fn device_info(a: &Adapter, address: Address) -> Result<DeviceInfo> {
    let d = a.device(address).map_err(err)?;
    let (name, alias, icon, class, appearance, paired, connected, trusted, blocked, rssi) =
        tokio::try_join!(
            d.name(),
            d.alias(),
            d.icon(),
            d.class(),
            d.appearance(),
            d.is_paired(),
            d.is_connected(),
            d.is_trusted(),
            d.is_blocked(),
            d.rssi(),
        )
        .map_err(err)?;
    // org.bluez.Battery1 only exists while a device that reports its battery is connected.
    let battery = if connected {
        d.battery_percentage().await.ok().flatten()
    } else {
        None
    };
    Ok(DeviceInfo {
        address,
        name,
        alias,
        icon,
        class,
        appearance,
        paired,
        connected,
        trusted,
        blocked,
        rssi,
        battery,
    })
}

/// All devices BlueZ knows on `a` (devices that vanish mid-query are skipped).
pub async fn devices(a: &Adapter) -> Result<Vec<DeviceInfo>> {
    let addrs = a.device_addresses().await.map_err(err)?;
    let infos =
        futures_util::future::join_all(addrs.into_iter().map(|addr| device_info(a, addr))).await;
    Ok(infos.into_iter().filter_map(Result::ok).collect())
}

pub struct Snapshot {
    pub adapter: AdapterInfo,
    pub devices: Vec<DeviceInfo>,
}

/// The BlueZ daemon (`bluetoothd`) is not running or not installed.
#[derive(Debug)]
pub struct NoBluez;

impl std::fmt::Display for NoBluez {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Bluetooth service (bluetoothd) is not running")
    }
}

impl std::error::Error for NoBluez {}

/// Whether a D-Bus error means nobody owns `org.bluez`.
fn is_no_bluez(e: &bluer::Error) -> bool {
    matches!(&e.kind, ErrorKind::Internal(InternalErrorKind::DBus(name))
        if name == "org.freedesktop.DBus.Error.ServiceUnknown"
            || name == "org.freedesktop.DBus.Error.NameHasNoOwner"
            || name.starts_with("org.freedesktop.systemd1."))
}

/// `Ok(None)` when BlueZ runs but there is no adapter; a [`NoBluez`] error when
/// BlueZ isn't running.
pub async fn snapshot() -> Result<Option<Snapshot>> {
    let a = match session().await?.default_adapter().await {
        Ok(a) => a,
        Err(e) if e.kind == ErrorKind::NotFound => return Ok(None),
        Err(e) if is_no_bluez(&e) => return Err(NoBluez.into()),
        Err(e) => return Err(err(e)),
    };
    let (adapter, devices) = tokio::try_join!(adapter_info(&a), devices(&a))?;
    Ok(Some(Snapshot { adapter, devices }))
}

pub async fn refetch(address: Address) -> Result<DeviceInfo> {
    device_info(&adapter().await?, address).await
}

/// Alias of a device, falling back to its address.
pub async fn device_label(address: Address) -> String {
    let alias = async {
        adapter()
            .await?
            .device(address)
            .map_err(err)?
            .alias()
            .await
            .map_err(err)
    };
    alias.await.unwrap_or_else(|_| address.to_string())
}

// ---- actions -------------------------------------------------------------

pub async fn connect(address: Address) -> Result<()> {
    let d = adapter().await?.device(address).map_err(err)?;
    match d.connect().await {
        Err(e) if e.kind != ErrorKind::AlreadyConnected => Err(err(e)),
        _ => Ok(()),
    }
}

pub async fn disconnect(address: Address) -> Result<()> {
    adapter()
        .await?
        .device(address)
        .map_err(err)?
        .disconnect()
        .await
        .map_err(err)
}

pub async fn set_trusted(address: Address, on: bool) -> Result<()> {
    adapter()
        .await?
        .device(address)
        .map_err(err)?
        .set_trusted(on)
        .await
        .map_err(err)
}

pub async fn set_blocked(address: Address, on: bool) -> Result<()> {
    adapter()
        .await?
        .device(address)
        .map_err(err)?
        .set_blocked(on)
        .await
        .map_err(err)
}

/// Empty `alias` resets to the device's own name.
pub async fn set_alias(address: Address, alias: String) -> Result<()> {
    adapter()
        .await?
        .device(address)
        .map_err(err)?
        .set_alias(alias)
        .await
        .map_err(err)
}

/// Remove the device and its pairing keys.
pub async fn forget(address: Address) -> Result<()> {
    adapter().await?.remove_device(address).await.map_err(err)
}

/// Outcome of [`pair`]: pairing errors abort; trust/connect errors are reported
/// separately because the device is usable (paired) even when they fail.
pub struct PairOutcome {
    pub connect_error: Option<anyhow::Error>,
}

/// Pair with `agent` registered on our session for the duration of the pairing,
/// then trust and connect. BlueZ routes the pairing requests of a `Pair()` call to
/// the agent registered by the caller's D-Bus connection, so this agent never needs
/// to become the default agent (another program may own that role for incoming requests).
/// Dropping the future cancels the pairing.
pub async fn pair(address: Address, agent: Agent) -> Result<PairOutcome> {
    let session = session().await?;
    let a = adapter().await?;
    let d = a.device(address).map_err(err)?;
    let _agent = session
        .register_agent(agent)
        .await
        .map_err(err)
        .context("cannot register pairing agent")?;
    tracing::info!("pairing agent registered; pairing {address}");
    match d.pair().await {
        Err(e) if e.kind != ErrorKind::AlreadyExists => return Err(err(e)),
        _ => {}
    }
    d.set_trusted(true).await.map_err(err)?;
    let connect_error = match d.connect().await {
        Err(e) if e.kind != ErrorKind::AlreadyConnected => Some(err(e)),
        _ => None,
    };
    Ok(PairOutcome { connect_error })
}

pub async fn set_powered(on: bool) -> Result<()> {
    adapter().await?.set_powered(on).await.map_err(err)
}

pub async fn set_discoverable(on: bool) -> Result<()> {
    adapter().await?.set_discoverable(on).await.map_err(err)
}

pub async fn set_pairable(on: bool) -> Result<()> {
    adapter().await?.set_pairable(on).await.map_err(err)
}

/// Run a discovery session for at most `limit`; returns when it ends. Dropping the
/// future (task abort) stops our discovery session.
pub async fn scan(limit: Duration) -> Result<()> {
    let a = adapter().await?;
    if !a.is_powered().await.map_err(err)? {
        return Err(anyhow!("turn Bluetooth on before scanning"));
    }
    let mut events = pin!(a.discover_devices().await.map_err(err)?);
    // Device changes are delivered through `watch`; this stream only keeps the session alive.
    let _ = tokio::time::timeout(limit, async { while events.next().await.is_some() {} }).await;
    Ok(())
}

// ---- change feed ---------------------------------------------------------

#[derive(Debug)]
pub enum Change {
    /// (Re)subscribed or the adapter changed: reload everything.
    Reset,
    Adapter(AdapterProperty),
    /// New device object, or one whose interfaces changed: re-read it.
    Added(Address),
    Removed(Address),
    Device(Address, DeviceProperty),
}

type DeviceStream = Pin<Box<dyn Stream<Item = (Address, DeviceEvent)> + Send>>;

/// Feed adapter/device changes into `tx` until the receiver is dropped.
pub async fn watch(tx: Sender<Change>) {
    loop {
        let session = match session().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("bluetooth watch: {e:#}");
                if tx.send(Change::Reset).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let session_events = session.events().await;
        match session.default_adapter().await {
            Ok(a) => {
                if let Err(e) = watch_adapter(&a, &tx).await {
                    tracing::warn!("bluetooth watch: {e:#}");
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(_) => {
                if tx.send(Change::Reset).await.is_err() {
                    return;
                }
                // Wait for an adapter to appear (re-check periodically in case events fail).
                let wait = async {
                    if let Ok(events) = session_events {
                        let mut events = pin!(events);
                        while let Some(ev) = events.next().await {
                            if matches!(ev, SessionEvent::AdapterAdded(_)) {
                                return;
                            }
                        }
                    }
                    std::future::pending::<()>().await;
                };
                let _ = tokio::time::timeout(Duration::from_secs(30), wait).await;
            }
        }
    }
}

async fn subscribe(
    a: &Adapter,
    addr: Address,
    streams: &mut SelectAll<DeviceStream>,
    subscribed: &mut HashSet<Address>,
) {
    if !subscribed.insert(addr) {
        return;
    }
    let Ok(device) = a.device(addr) else { return };
    match device.events().await {
        Ok(ev) => streams.push(Box::pin(ev.map(move |e| (addr, e)))),
        Err(e) => {
            subscribed.remove(&addr);
            tracing::warn!("cannot watch {addr}: {e}");
        }
    }
}

async fn watch_adapter(a: &Adapter, tx: &Sender<Change>) -> Result<()> {
    let mut events = pin!(a.events().await.map_err(err)?);
    let mut streams = SelectAll::<DeviceStream>::new();
    let mut subscribed = HashSet::new();
    for addr in a.device_addresses().await.map_err(err)? {
        subscribe(a, addr, &mut streams, &mut subscribed).await;
    }
    // Subscribed first, so nothing between the consumer's reload and our events is lost.
    if tx.send(Change::Reset).await.is_err() {
        return Ok(());
    }
    loop {
        let change = tokio::select! {
            ev = events.next() => match ev {
                None => return Ok(()),
                Some(AdapterEvent::DeviceAdded(addr)) => {
                    subscribe(a, addr, &mut streams, &mut subscribed).await;
                    Change::Added(addr)
                }
                Some(AdapterEvent::DeviceRemoved(addr)) => {
                    // bluer reports *any* InterfacesRemoved on the device path (e.g. Battery1
                    // going away on disconnect) as removal and ends that device's event
                    // stream; re-subscribe if the device itself still exists.
                    subscribed.remove(&addr);
                    let exists = match a.device(addr) {
                        Ok(d) => d.alias().await.is_ok(),
                        Err(_) => false,
                    };
                    if exists {
                        subscribe(a, addr, &mut streams, &mut subscribed).await;
                        Change::Added(addr)
                    } else {
                        Change::Removed(addr)
                    }
                }
                Some(AdapterEvent::PropertyChanged(p)) => Change::Adapter(p),
            },
            Some((addr, DeviceEvent::PropertyChanged(p))) = streams.next(), if !streams.is_empty() => {
                Change::Device(addr, p)
            }
        };
        if tx.send(change).await.is_err() {
            return Ok(());
        }
    }
}

// ---- rfkill --------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rfkill {
    pub soft: bool,
    pub hard: bool,
}

/// Bluetooth rfkill state from sysfs (blocking; tiny reads).
pub fn rfkill() -> Rfkill {
    let mut state = Rfkill::default();
    let Ok(dir) = std::fs::read_dir("/sys/class/rfkill") else {
        return state;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let read = |f: &str| std::fs::read_to_string(path.join(f)).unwrap_or_default();
        if read("type").trim() != "bluetooth" {
            continue;
        }
        state.soft |= read("soft").trim() == "1";
        state.hard |= read("hard").trim() == "1";
    }
    state
}

/// `rfkill unblock bluetooth` (user is in the rfkill group; no root needed). Blocking.
pub fn rfkill_unblock() -> Result<()> {
    hyprdeck_core::cmd::run("rfkill", ["unblock", "bluetooth"]).map(drop)
}
