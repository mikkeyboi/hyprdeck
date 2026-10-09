//! Centralized notification policy and desktop delivery via `org.freedesktop.Notifications`.
//! The desktop owns presentation, do-not-disturb and history; delivery never opens a window.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{OnceCell, watch};
use zbus::message::Sequence;
use zbus::zvariant::Value;
use zbus::{Connection, MessageStream};

const SERVICE: &str = "org.freedesktop.Notifications";
const SERVICE_PATH: &str = "/org/freedesktop/Notifications";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const SETTINGS_FILE: &str = "notifications.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    PackageUpdates,
    HyprdeckUpdates,
    Audio,
    Bluetooth,
    System,
}

impl Category {
    pub const ALL: [Self; 5] = [
        Self::PackageUpdates,
        Self::HyprdeckUpdates,
        Self::Audio,
        Self::Bluetooth,
        Self::System,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::PackageUpdates => "Package updates",
            Self::HyprdeckUpdates => "Hyprdeck updates",
            Self::Audio => "Audio",
            Self::Bluetooth => "Bluetooth",
            Self::System => "System",
        }
    }

    fn is_update(self) -> bool {
        matches!(self, Self::PackageUpdates | Self::HyprdeckUpdates)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Normal,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Desktop,
    InApp,
    Off,
}

impl Delivery {
    pub const ALL: [Self; 3] = [Self::Desktop, Self::InApp, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop",
            Self::InApp => "In-app only",
            Self::Off => "Off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupTimeout {
    #[default]
    Timed,
    ServerDefault,
    UntilDismissed,
}

impl PopupTimeout {
    pub const ALL: [Self; 3] = [Self::Timed, Self::ServerDefault, Self::UntilDismissed];

    pub fn label(self) -> &'static str {
        match self {
            Self::Timed => "Timed",
            Self::ServerDefault => "Desktop default",
            Self::UntilDismissed => "Until dismissed",
        }
    }
}

/// `~/.config/hyprdeck/notifications.toml`. An absent override inherits `delivery`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub delivery: Delivery,
    pub timeout: PopupTimeout,
    pub duration_seconds: u32,
    pub error_duration_seconds: u32,
    pub overrides: BTreeMap<Category, Delivery>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            delivery: Delivery::Desktop,
            timeout: PopupTimeout::Timed,
            duration_seconds: 8,
            error_duration_seconds: 12,
            overrides: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<()> {
        for (name, seconds) in [
            ("duration_seconds", self.duration_seconds),
            ("error_duration_seconds", self.error_duration_seconds),
        ] {
            ensure!(
                (1..=3600).contains(&seconds),
                "{name} must be between 1 and 3600"
            );
        }
        Ok(())
    }

    fn route(&self, category: Category, visible: bool) -> Route {
        match self
            .overrides
            .get(&category)
            .copied()
            .unwrap_or(self.delivery)
        {
            Delivery::Off => Route::Suppressed,
            Delivery::Desktop if !visible => Route::Desktop,
            _ if visible => Route::Toast,
            _ => Route::Suppressed,
        }
    }

    fn expiry(&self, severity: Severity) -> i32 {
        match self.timeout {
            PopupTimeout::ServerDefault => -1,
            PopupTimeout::UntilDismissed => 0,
            PopupTimeout::Timed => {
                let seconds = match severity {
                    Severity::Normal => self.duration_seconds,
                    Severity::Error => self.error_duration_seconds,
                };
                // Settings are validated before entering the cache (3600 seconds fits i32).
                (seconds * 1000) as i32
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Desktop,
    Toast,
    Suppressed,
}

static SETTINGS: LazyLock<Mutex<Option<Settings>>> = LazyLock::new(|| Mutex::new(None));
static SESSION: LazyLock<OnceCell<Connection>> = LazyLock::new(OnceCell::new);

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn decode_settings(text: &str, path: &Path) -> Result<Settings> {
    let settings: Settings =
        toml::from_str(text).with_context(|| format!("invalid {}", path.display()))?;
    settings
        .validate()
        .with_context(|| format!("invalid {}", path.display()))?;
    Ok(settings)
}

fn legacy_disabled(dir: &Path, name: &str, keys: &[&str]) -> Result<bool> {
    let path = dir.join(format!("{name}.toml"));
    let Some(text) = read_optional(&path)? else {
        return Ok(false);
    };
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    let mut value = &value;
    for key in keys {
        let table = value
            .as_table()
            .with_context(|| format!("invalid {}: expected a table", path.display()))?;
        let Some(next) = table.get(*key) else {
            return Ok(false);
        };
        value = next;
    }
    let enabled = value.as_bool().with_context(|| {
        format!(
            "invalid {}: {} must be a boolean",
            path.display(),
            keys.join(".")
        )
    })?;
    Ok(!enabled)
}

fn load_from(dir: &Path) -> Result<Settings> {
    let path = dir.join(SETTINGS_FILE);
    if let Some(text) = read_optional(&path)? {
        return decode_settings(&text, &path);
    }
    let mut settings = Settings::default();
    for (name, keys, category) in [
        ("audio", &["notifications"][..], Category::Audio),
        ("updates", &["notify"][..], Category::PackageUpdates),
        ("system", &["resume", "notify"][..], Category::System),
    ] {
        if legacy_disabled(dir, name, keys)? {
            settings.overrides.insert(category, Delivery::Off);
        }
    }
    // Install once without clobbering a config created by another process during migration.
    // Hard-linking the complete, same-directory candidate keeps readers from seeing partial TOML.
    let candidate = dir.join(format!(".notifications.migration-{}", std::process::id()));
    let text = toml::to_string_pretty(&settings).context("serializing notification settings")?;
    crate::store::write_atomic(&candidate, text.as_bytes())?;
    let installed = std::fs::hard_link(&candidate, &path);
    let cleanup = std::fs::remove_file(&candidate);
    match installed {
        Ok(()) => {
            cleanup.with_context(|| format!("removing {}", candidate.display()))?;
            Ok(settings)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            cleanup.with_context(|| format!("removing {}", candidate.display()))?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            decode_settings(&text, &path)
        }
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

/// Load and validate settings, migrating retired notification switches only when absent.
/// Blocking file I/O; errors leave existing files untouched.
pub fn load_settings() -> Result<Settings> {
    let mut cache = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let settings = load_from(&crate::store::config_dir())?;
    *cache = Some(settings.clone());
    Ok(settings)
}

/// Cached settings. An invalid/unreadable initial configuration suppresses delivery until a
/// successful explicit load or save; it is never silently replaced by a default file.
pub fn settings() -> Settings {
    let mut cache = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .get_or_insert_with(|| {
            load_from(&crate::store::config_dir()).unwrap_or_else(|e| {
                tracing::warn!("notification settings: {e:#}");
                Settings {
                    delivery: Delivery::Off,
                    ..Settings::default()
                }
            })
        })
        .clone()
}

fn save_to(dir: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    let path = dir.join(SETTINGS_FILE);
    if let Some(text) = read_optional(&path)? {
        // Do not erase a malformed file, even when called before the preferences UI loads it.
        decode_settings(&text, &path)?;
    }
    let text = toml::to_string_pretty(settings).context("serializing notification settings")?;
    crate::store::write_atomic(&path, text.as_bytes())
}

/// Validate and atomically persist settings before updating the cache. Blocking file I/O.
pub fn save_settings(settings: Settings) -> Result<()> {
    let mut cache = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    save_to(&crate::store::config_dir(), &settings)?;
    *cache = Some(settings);
    Ok(())
}

/// Shared session-bus connection (lazily opened on the tokio runtime).
pub async fn session_bus() -> Result<Connection> {
    let conn = SESSION
        .get_or_try_init(|| async { Connection::session().await })
        .await?;
    Ok(conn.clone())
}

// Sending update popups is serialized, but signal processing never waits for this lock.
static UPDATE_SEND: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
static UPDATE_POPUPS: LazyLock<Mutex<BTreeMap<Category, Popup>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

struct Popup {
    id: u32,
    owner: Arc<str>,
    lease: Arc<Lease>,
}

struct Lease {
    live: AtomicBool,
    cancelled: watch::Sender<bool>,
    deadline: Option<Instant>,
}

impl Lease {
    fn new(expiry: i32) -> Self {
        Self {
            live: AtomicBool::new(true),
            cancelled: watch::Sender::new(false),
            deadline: (expiry > 0).then(|| Instant::now() + Duration::from_millis(expiry as u64)),
        }
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
            && self
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn cancel(&self) -> bool {
        let live = self.live.swap(false, Ordering::AcqRel);
        self.cancelled.send_replace(true);
        live && self
            .deadline
            .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn finish(&self, clicked: bool) -> bool {
        // Replacement, expiry, closure and a click have exactly one winner. In particular,
        // an already queued ActionInvoked cannot revive a replaced or expired action.
        let live = self.live.swap(false, Ordering::AcqRel);
        clicked
            && live
            && self
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }
}

fn take_previous(category: Category) -> Option<(Popup, bool)> {
    if !category.is_update() {
        return None;
    }
    let previous = UPDATE_POPUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&category)?;
    let live = previous.lease.cancel();
    Some((previous, live))
}

async fn owner_reply(conn: &Connection) -> Result<zbus::Message> {
    Ok(conn
        .call_method(Some(BUS), BUS_PATH, Some(BUS), "GetNameOwner", &(SERVICE,))
        .await?)
}

async fn send(
    conn: &Connection,
    owner: &str,
    replaces: u32,
    severity: Severity,
    expiry: i32,
    content: (&str, &str),
    actions: &[&str],
) -> Result<(u32, Sequence)> {
    let (summary, body) = content;
    let mut hints: HashMap<&str, Value<'_>> = HashMap::with_capacity(3);
    hints.insert(
        "urgency",
        Value::from(if severity == Severity::Error {
            2u8
        } else {
            1u8
        }),
    );
    if severity == Severity::Normal {
        hints.insert("suppress-sound", Value::from(true));
    }
    if expiry > 0 {
        hints.insert("resident", Value::from(false));
    }
    let reply = conn
        .call_method(
            Some(owner),
            SERVICE_PATH,
            Some(SERVICE),
            "Notify",
            &(
                "Hyprdeck",
                replaces,
                crate::APP_ICON,
                summary,
                body,
                actions,
                hints,
                expiry,
            ),
        )
        .await?;
    ensure!(
        reply
            .header()
            .sender()
            .is_some_and(|sender| sender.as_str() == owner),
        "notification reply came from an unexpected sender"
    );
    let id = reply.body().deserialize::<u32>()?;
    ensure!(
        id != 0,
        "notification service returned an invalid notification ID"
    );
    Ok((id, reply.recv_position()))
}

struct Watcher {
    signals: MessageStream,
    owners: MessageStream,
    owner: Arc<str>,
    id: u32,
    after: Sequence,
    actionable: bool,
    lease: Arc<Lease>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Aborting a caller must not leave a supposedly live replacement ID behind.
        self.lease.cancel();
    }
}

impl Watcher {
    async fn wait(mut self) -> Result<bool> {
        let mut cancelled = self.lease.cancelled.subscribe();
        let deadline = self.lease.deadline;
        let expiry = async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(expiry);
        let result = async {
            loop {
                if !self.lease.is_live() || *cancelled.borrow() {
                    return Ok(false);
                }
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => return Ok(false),
                    _ = &mut expiry => return Ok(false),
                    owner = self.owners.next() => {
                        let Some(owner) = owner else { return Ok(false); };
                        let owner = owner?;
                        if owner.header().sender().is_none_or(|sender| sender.as_str() != BUS) {
                            continue;
                        }
                        let body = owner.body();
                        let (name, old, new): (&str, &str, &str) = body.deserialize()?;
                        if name == SERVICE && old == self.owner.as_ref() && new != old {
                            return Ok(false);
                        }
                    }
                    signal = self.signals.next() => {
                        let Some(signal) = signal else { return Ok(false); };
                        let signal = signal?;
                        let header = signal.header();
                        if signal.recv_position() <= self.after
                            || header.sender().is_none_or(|sender| sender.as_str() != self.owner.as_ref())
                        {
                            continue;
                        }
                        match header.member().map(|member| member.as_str()) {
                            Some("ActionInvoked") if self.actionable => {
                                let body = signal.body();
                                let (id, action): (u32, &str) = body.deserialize()?;
                                if id == self.id && (action == "run" || action == "default") {
                                    return Ok(true);
                                }
                            }
                            Some("NotificationClosed") => {
                                let (id, _reason): (u32, u32) = signal.body().deserialize()?;
                                if id == self.id {
                                    return Ok(false);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }.await;
        match result {
            Ok(clicked) => Ok(self.lease.finish(clicked)),
            Err(error) => {
                self.lease.finish(false);
                Err(error)
            }
        }
    }
}

async fn deliver(
    category: Category,
    severity: Severity,
    summary: &str,
    body: &str,
    action_label: Option<&str>,
) -> Result<Option<Watcher>> {
    let settings = settings();
    let _sending = if category.is_update() {
        Some(UPDATE_SEND.lock().await)
    } else {
        None
    };
    // Every new update offer invalidates the previous action, even when policy now suppresses it.
    let previous = take_previous(category);
    match settings.route(category, crate::ui::window_visible()) {
        Route::Suppressed => return Ok(None),
        Route::Toast => {
            let text = if body.is_empty() {
                summary.to_owned()
            } else {
                format!("{summary}: {body}")
            };
            crate::events::send(crate::events::AppEvent::Toast(text));
            return Ok(None);
        }
        Route::Desktop => {}
    }
    let conn = session_bus().await?;
    let (owner, mut after) = {
        let reply = owner_reply(&conn).await?;
        let body = reply.body();
        let owner: &str = body.deserialize()?;
        ensure!(
            owner.starts_with(':'),
            "notification service has no unique owner"
        );
        (Arc::<str>::from(owner), reply.recv_position())
    };
    let monitored = action_label.is_some() || category.is_update();
    let streams = if monitored {
        // Match the actual unique owner, not a spoofable interface/path or a moving well-known name.
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(owner.as_ref())?
            .interface(SERVICE)?
            .path(SERVICE_PATH)?
            .build();
        let signals = MessageStream::for_match_rule(rule, &conn, Some(32)).await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BUS)?
            .interface(BUS)?
            .path(BUS_PATH)?
            .member("NameOwnerChanged")?
            .arg(0, SERVICE)?
            .build();
        let owners = MessageStream::for_match_rule(rule, &conn, Some(8)).await?;
        // A barrier after subscription excludes old queued events and catches an owner switch
        // during setup. New-ID clicks between Notify and its reply still remain observable.
        let reply = owner_reply(&conn).await?;
        let body = reply.body();
        let current: &str = body.deserialize()?;
        ensure!(
            current == owner.as_ref(),
            "notification service changed during subscription"
        );
        after = reply.recv_position();
        Some((signals, owners))
    } else {
        None
    };
    let replaces = previous
        .as_ref()
        .filter(|(popup, live)| {
            *live
                && popup.owner == owner
                && popup
                    .lease
                    .deadline
                    .is_none_or(|deadline| Instant::now() < deadline)
        })
        .map_or(0, |(popup, _)| popup.id);
    let expiry = settings.expiry(severity);
    let lease = monitored.then(|| Arc::new(Lease::new(expiry)));
    let action_pairs = action_label.map(|label| ["default", label, "run", label]);
    let actions = action_pairs.as_ref().map_or(&[][..], |pairs| &pairs[..]);
    let (id, reply_position) = send(
        &conn,
        &owner,
        replaces,
        severity,
        expiry,
        (summary, body),
        actions,
    )
    .await?;
    let Some((signals, owners)) = streams else {
        return Ok(None);
    };
    let lease = lease.expect("monitored delivery has a lease");
    if id == replaces {
        // Reused IDs make pre-reply signals ambiguous: conservatively reject old clicks/closures.
        after = reply_position;
    }
    if category.is_update() {
        UPDATE_POPUPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                category,
                Popup {
                    id,
                    owner: owner.clone(),
                    lease: lease.clone(),
                },
            );
    }
    Ok(Some(Watcher {
        signals,
        owners,
        owner,
        id,
        after,
        actionable: action_label.is_some(),
        lease,
    }))
}

/// Deliver according to policy. Must run on the tokio runtime (use `rt::spawn`).
pub async fn notify(
    category: Category,
    severity: Severity,
    summary: &str,
    body: &str,
) -> Result<()> {
    if let Some(watcher) = deliver(category, severity, summary, body, None).await? {
        crate::rt::spawn(async move {
            if let Err(e) = watcher.wait().await {
                tracing::warn!("notification watcher failed: {e:#}");
            }
        });
    }
    Ok(())
}

/// Deliver with an explicit button and advertised body/default action. Returns `true` only
/// for a live user click, never on expiry, dismissal, suppression or in-app delivery.
pub async fn notify_action(
    category: Category,
    severity: Severity,
    summary: &str,
    body: &str,
    action_label: &str,
) -> Result<bool> {
    match deliver(category, severity, summary, body, Some(action_label)).await? {
        Some(watcher) => watcher.wait().await,
        None => Ok(false),
    }
}

/// Fire-and-forget notification from any thread, using the same policy as actionable delivery.
pub fn notify_bg(
    category: Category,
    severity: Severity,
    summary: impl Into<String>,
    body: impl Into<String>,
) {
    let (summary, body) = (summary.into(), body.into());
    crate::rt::spawn(async move {
        if let Err(e) = notify(category, severity, &summary, &body).await {
            tracing::warn!("notification failed: {e:#}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    struct ConfigDir(PathBuf);

    impl ConfigDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "hyprdeck-notifications-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn put(&self, name: &str, text: &str) {
            std::fs::write(self.0.join(name), text).unwrap();
        }
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_and_duration_boundaries() {
        let settings: Settings = toml::from_str("").unwrap();
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.expiry(Severity::Normal), 8000);
        assert_eq!(settings.expiry(Severity::Error), 12000);
        for seconds in [1, 3600] {
            let settings = Settings {
                duration_seconds: seconds,
                error_duration_seconds: seconds,
                ..Settings::default()
            };
            assert!(settings.validate().is_ok());
            assert_eq!(settings.expiry(Severity::Error), (seconds * 1000) as i32);
        }
        for seconds in [0, 3601, u32::MAX] {
            assert!(
                Settings {
                    duration_seconds: seconds,
                    ..Settings::default()
                }
                .validate()
                .is_err()
            );
            assert!(
                Settings {
                    error_duration_seconds: seconds,
                    timeout: PopupTimeout::UntilDismissed,
                    ..Settings::default()
                }
                .validate()
                .is_err()
            );
        }
        for (timeout, expiry) in [
            (PopupTimeout::ServerDefault, -1),
            (PopupTimeout::UntilDismissed, 0),
        ] {
            let settings = Settings {
                timeout,
                ..Settings::default()
            };
            assert_eq!(settings.expiry(Severity::Normal), expiry);
            assert_eq!(settings.expiry(Severity::Error), expiry);
        }
    }

    #[test]
    fn delivery_overrides_and_visibility_take_precedence() {
        for delivery in Delivery::ALL {
            let mut settings = Settings {
                delivery,
                ..Settings::default()
            };
            for category in Category::ALL {
                let hidden = match delivery {
                    Delivery::Desktop => Route::Desktop,
                    _ => Route::Suppressed,
                };
                let visible = match delivery {
                    Delivery::Off => Route::Suppressed,
                    _ => Route::Toast,
                };
                assert_eq!(settings.route(category, false), hidden);
                assert_eq!(settings.route(category, true), visible);
                settings.overrides.insert(category, Delivery::Off);
                assert_eq!(settings.route(category, true), Route::Suppressed);
                settings.overrides.insert(category, Delivery::Desktop);
                assert_eq!(settings.route(category, false), Route::Desktop);
                assert_eq!(settings.route(category, true), Route::Toast);
                settings.overrides.insert(category, Delivery::InApp);
                assert_eq!(settings.route(category, false), Route::Suppressed);
                assert_eq!(settings.route(category, true), Route::Toast);
                settings.overrides.remove(&category);
            }
        }
    }

    #[test]
    fn custom_settings_persist_and_legacy_keys_are_read_only_once() {
        let dir = ConfigDir::new();
        dir.put("audio.toml", "notifications = false\n");
        dir.put(
            "updates.toml",
            "notify = false\nself_update_policy = 'download'\n",
        );
        dir.put("system.toml", "[resume]\nnotify = false\n");
        let migrated = load_from(&dir.0).unwrap();
        for category in [Category::Audio, Category::PackageUpdates, Category::System] {
            assert_eq!(migrated.overrides.get(&category), Some(&Delivery::Off));
        }
        assert!(!migrated.overrides.contains_key(&Category::HyprdeckUpdates));
        assert_eq!(
            std::fs::read_to_string(dir.0.join("updates.toml")).unwrap(),
            "notify = false\nself_update_policy = 'download'\n"
        );
        dir.put("audio.toml", "this is no longer valid TOML");
        assert_eq!(load_from(&dir.0).unwrap(), migrated);
        for timeout in PopupTimeout::ALL {
            let settings = Settings {
                delivery: Delivery::InApp,
                timeout,
                duration_seconds: 1,
                error_duration_seconds: 3600,
                overrides: BTreeMap::from([
                    (Category::Bluetooth, Delivery::Desktop),
                    (Category::HyprdeckUpdates, Delivery::Off),
                ]),
            };
            save_to(&dir.0, &settings).unwrap();
            assert_eq!(load_from(&dir.0).unwrap(), settings);
        }
    }

    #[test]
    fn missing_or_enabled_legacy_preferences_keep_defaults() {
        let dir = ConfigDir::new();
        assert_eq!(load_from(&dir.0).unwrap(), Settings::default());
        let dir = ConfigDir::new();
        dir.put("audio.toml", "notifications = true\n");
        dir.put(
            "updates.toml",
            "notify = true\nself_update_policy = 'off'\n",
        );
        dir.put("system.toml", "[resume]\nnotify = true\n");
        assert_eq!(load_from(&dir.0).unwrap(), Settings::default());
    }

    #[test]
    fn malformed_configuration_is_not_replaced() {
        for invalid in [
            "[",
            "duration_seconds = 0",
            "duration_seconds = -1",
            "duration_seconds = 4294967296",
            "error_duration_seconds = 4294967295",
            "delivery = 'unknown'",
            "[overrides]\nunknown = 'off'",
        ] {
            let dir = ConfigDir::new();
            dir.put(SETTINGS_FILE, invalid);
            assert!(load_from(&dir.0).is_err());
            assert!(save_to(&dir.0, &Settings::default()).is_err());
            assert_eq!(
                std::fs::read_to_string(dir.0.join(SETTINGS_FILE)).unwrap(),
                invalid
            );
        }
        for (name, invalid) in [
            ("audio.toml", "notifications = 'false'"),
            ("updates.toml", "["),
            ("system.toml", "resume = false"),
        ] {
            let dir = ConfigDir::new();
            dir.put(name, invalid);
            assert!(load_from(&dir.0).is_err());
            assert!(!dir.0.join(SETTINGS_FILE).exists());
            assert_eq!(std::fs::read_to_string(dir.0.join(name)).unwrap(), invalid);
        }
        let dir = ConfigDir::new();
        assert!(
            save_to(
                &dir.0,
                &Settings {
                    duration_seconds: 0,
                    ..Settings::default()
                }
            )
            .is_err()
        );
        assert!(!dir.0.join(SETTINGS_FILE).exists());
    }

    #[test]
    fn expiry_dismissal_and_replacement_cannot_execute_actions() {
        let expired = Lease {
            live: AtomicBool::new(true),
            cancelled: watch::Sender::new(false),
            deadline: Some(Instant::now()),
        };
        assert!(!expired.is_live());
        assert!(!expired.finish(true));
        let dismissed = Lease::new(0);
        assert!(!dismissed.finish(false));
        assert!(!dismissed.finish(true));
        let replaced = Lease::new(8000);
        replaced.cancel();
        assert!(!replaced.finish(true));
        let current = Lease::new(8000);
        assert!(current.finish(true));
        assert!(!current.finish(true));
    }

    #[test]
    fn concurrent_clicks_have_only_one_winner() {
        let lease = Arc::new(Lease::new(8000));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let lease = lease.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    lease.finish(true)
                })
            })
            .collect();
        barrier.wait();
        let wins = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().then_some(()))
            .count();
        assert_eq!(wins, 1);
    }
}
