//! PipeWire access through `pactl` (pipewire-pulse).
//!
//! Every mutation is an explicit argv list (never re-parsed by a shell) so the
//! builders are unit tested without a sound server.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use tokio::process::Command;

pub const MAX_VOLUME: u32 = 150;
pub const PRIMARY_SINK: &str = "simultaneous_output";
pub const PRIMARY_DESCRIPTION: &str = "Simultaneous-Outputs";
pub const ROUTE_PREFIX: &str = "simultaneous_route_";
const IGNORED_SINKS: &[&str] = &["auto_null"];
const TIMEOUT: Duration = Duration::from_secs(6);

/// How an output is attached; also the UI grouping/sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Conn {
    Virtual,
    Bluetooth,
    Usb,
    Hdmi,
    Analog,
}

impl Conn {
    pub fn label(self) -> &'static str {
        match self {
            Conn::Virtual => "Virtual",
            Conn::Bluetooth => "Bluetooth",
            Conn::Usb => "USB",
            Conn::Hdmi => "HDMI",
            Conn::Analog => "Analog",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Conn::Virtual => "media-playlist-shuffle-symbolic",
            Conn::Bluetooth => "bluetooth-symbolic",
            Conn::Usb => "drive-removable-media-symbolic",
            Conn::Hdmi => "video-display-symbolic",
            Conn::Analog => "audio-speakers-symbolic",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sink {
    pub name: String,
    pub index: u32,
    pub description: String,
    /// Human title; disambiguated with the port name when two outputs share a description.
    pub label: String,
    pub conn: Conn,
    pub volume: u32,
    pub muted: bool,
    pub is_default: bool,
    /// `RUNNING`, `IDLE` or `SUSPENDED`.
    pub state: String,
    /// `(card name, active port)` when the output belongs to a card port; needed for latency offsets.
    pub port: Option<(String, String)>,
    /// Port latency offset in microseconds (filled from the card list).
    pub latency_offset_us: Option<i64>,
}

impl Sink {
    pub fn is_primary(&self) -> bool {
        self.name == PRIMARY_SINK
    }
    pub fn is_route(&self) -> bool {
        self.name.starts_with(ROUTE_PREFIX)
    }
    pub fn is_virtual(&self) -> bool {
        is_virtual_name(&self.name)
    }
    pub fn is_running(&self) -> bool {
        self.state == "RUNNING"
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub index: u32,
    pub sink_index: u32,
    pub app_name: String,
    pub binary: String,
    pub media_name: String,
    pub icon: String,
    pub volume: u32,
    pub muted: bool,
    pub corked: bool,
    pub pid: i64,
}

impl Stream {
    pub fn label(&self) -> String {
        [&self.app_name, &self.binary, &self.media_name]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("Stream {}", self.index))
    }

    pub fn detail(&self) -> &str {
        if !self.media_name.is_empty() && self.media_name != self.app_name {
            &self.media_name
        } else {
            &self.binary
        }
    }
}

/// One of our own combine-sink member streams: `virtual_sink` feeds the sink at `member_index`.
#[derive(Debug, Clone, PartialEq)]
pub struct FanOut {
    pub virtual_sink: String,
    pub member_index: u32,
}

pub fn is_virtual_name(name: &str) -> bool {
    name == PRIMARY_SINK || name.starts_with(ROUTE_PREFIX)
}

pub fn route_sink_name(suffix: &str) -> String {
    format!("{ROUTE_PREFIX}{suffix}")
}

/// `media.name` module-combine-sink gives its per-member streams.
pub fn fanout_media_name(sink: &str) -> String {
    format!("{sink} output")
}

fn fanout_owner(media_name: &str) -> Option<&str> {
    media_name
        .strip_suffix(" output")
        .filter(|owner| is_virtual_name(owner))
}

fn prop<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get("properties")
        .and_then(|p| p.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn text<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}

fn index_of(item: &Value, key: &str) -> u32 {
    item.get(key)
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .map_or(u32::MAX, |v| v as u32)
}

pub fn connection_of(sink: &Value) -> Conn {
    let name = text(sink, "name");
    if is_virtual_name(name) || prop(sink, "node.virtual").eq_ignore_ascii_case("true") {
        return Conn::Virtual;
    }
    let bus = prop(sink, "device.bus").to_ascii_lowercase();
    let lower = name.to_ascii_lowercase();
    if bus == "bluetooth" || name.starts_with("bluez_output.") {
        Conn::Bluetooth
    } else if lower.contains("hdmi")
        || lower.contains("displayport")
        || prop(sink, "device.form_factor").eq_ignore_ascii_case("tv")
    {
        Conn::Hdmi
    } else if bus == "usb" || name.contains(".usb-") {
        Conn::Usb
    } else {
        Conn::Analog
    }
}

/// Average channel volume in percent, clamped to 0..=MAX_VOLUME.
pub fn volume_percent(item: &Value) -> u32 {
    let Some(channels) = item.get("volume").and_then(Value::as_object) else {
        return 0;
    };
    if channels.is_empty() {
        return 0;
    }
    let sum: f64 = channels
        .values()
        .map(|ch| {
            if let Some(p) = ch
                .get("value_percent")
                .and_then(Value::as_str)
                .and_then(|t| t.trim().strip_suffix('%'))
                && let Ok(v) = p.trim().parse::<f64>()
            {
                return v;
            }
            let raw = ch.get("value").and_then(Value::as_f64).unwrap_or(0.0);
            // pactl's `value` is fixed point (65536 == 100%); tiny values are 0..1 scalars.
            if raw <= 2.0 {
                raw * 100.0
            } else {
                raw * 100.0 / 65536.0
            }
        })
        .sum();
    (sum / channels.len() as f64)
        .round()
        .clamp(0.0, MAX_VOLUME as f64) as u32
}

/// Parse `pactl --format=json list sinks`; drops the dummy sink, sorts Virtual, Bluetooth, USB, HDMI, Analog.
pub fn parse_sinks(json: &Value, default_sink: &str) -> Vec<Sink> {
    let mut sinks: Vec<Sink> = json
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| !IGNORED_SINKS.contains(&text(s, "name")))
        .map(|s| {
            let name = text(s, "name").to_owned();
            let description = match text(s, "description") {
                "" => name.clone(),
                d => d.to_owned(),
            };
            let card = prop(s, "device.name");
            let active_port = text(s, "active_port");
            let port = (!card.is_empty() && !active_port.is_empty())
                .then(|| (card.to_owned(), active_port.to_owned()));
            Sink {
                label: String::new(),
                conn: connection_of(s),
                index: index_of(s, "index"),
                volume: volume_percent(s),
                muted: s.get("mute").and_then(Value::as_bool).unwrap_or(false),
                is_default: name == default_sink,
                state: text(s, "state").to_owned(),
                port,
                latency_offset_us: None,
                description,
                name,
            }
        })
        .collect();
    let port_desc = |s: &Value| -> Option<String> {
        let active = text(s, "active_port");
        s.get("ports")?
            .as_array()?
            .iter()
            .find(|p| text(p, "name") == active)
            .map(|p| text(p, "description").to_owned())
            .filter(|d| !d.is_empty())
    };
    let raw: HashMap<&str, &Value> = json
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| (text(s, "name"), s))
        .collect();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for s in &sinks {
        *seen.entry(s.description.clone()).or_default() += 1;
    }
    for s in &mut sinks {
        s.label = if s.is_primary() {
            "Simultaneous Outputs".to_owned()
        } else if s.is_route() {
            let l = s.description.replace("Route-", "").replace('-', " ");
            if l.is_empty() { "Route".to_owned() } else { l }
        } else if seen[&s.description] > 1
            && let Some(port) = raw.get(s.name.as_str()).and_then(|v| port_desc(v))
        {
            format!("{} · {port}", s.description)
        } else {
            s.description.clone()
        };
    }
    sinks.sort_by_cached_key(|s| (s.conn, s.label.to_lowercase()));
    sinks
}

/// Parse `pactl --format=json list sink-inputs` into application streams and our own fan-out streams.
pub fn parse_streams(json: &Value) -> (Vec<Stream>, Vec<FanOut>) {
    let mut streams = Vec::new();
    let mut fanout = Vec::new();
    for item in json.as_array().into_iter().flatten() {
        let media_name = prop(item, "media.name");
        if let Some(owner) = fanout_owner(media_name) {
            fanout.push(FanOut {
                virtual_sink: owner.to_owned(),
                member_index: index_of(item, "sink"),
            });
            continue;
        }
        streams.push(Stream {
            index: index_of(item, "index"),
            sink_index: index_of(item, "sink"),
            app_name: prop(item, "application.name").to_owned(),
            binary: prop(item, "application.process.binary").to_owned(),
            media_name: media_name.to_owned(),
            icon: prop(item, "application.icon_name").to_owned(),
            volume: volume_percent(item),
            muted: item.get("mute").and_then(Value::as_bool).unwrap_or(false),
            corked: item.get("corked").and_then(Value::as_bool).unwrap_or(false),
            pid: prop(item, "application.process.id").parse().unwrap_or(-1),
        });
    }
    (streams, fanout)
}

/// `(card, port) -> latency offset µs` from `pactl --format=json list cards`.
pub fn parse_port_offsets(json: &Value) -> HashMap<(String, String), i64> {
    let mut out = HashMap::new();
    for card in json.as_array().into_iter().flatten() {
        let Some(ports) = card.get("ports").and_then(Value::as_object) else {
            continue;
        };
        for (port, info) in ports {
            let offset = match info.get("latency_offset") {
                Some(Value::Number(n)) => n.as_i64(),
                Some(Value::String(s)) => s.trim().trim_end_matches("usec").trim().parse().ok(),
                _ => None,
            };
            if let Some(offset) = offset {
                out.insert((text(card, "name").to_owned(), port.clone()), offset);
            }
        }
    }
    out
}

/// Module ids of every `module-combine-sink` owning `sink` in `pactl list short modules` output.
pub fn parse_module_ids(modules: &str, sink: &str) -> Vec<u32> {
    let prefix = format!("sink_name={sink}");
    modules
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f.len() >= 3
                && f[1] == "module-combine-sink"
                && (f[2] == prefix || f[2].starts_with(&format!("{prefix} "))))
            .then(|| f[0].trim().parse().ok())
            .flatten()
        })
        .collect()
}

/// The `sinks=` list a live combine module named `sink` was loaded with.
pub fn parse_combine_targets(modules: &str, sink: &str) -> Option<Vec<String>> {
    modules.lines().find_map(|line| {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 || f[1] != "module-combine-sink" {
            return None;
        }
        let args: HashMap<&str, &str> = f[2].split(' ').filter_map(|p| p.split_once('=')).collect();
        (args.get("sink_name") == Some(&sink)).then(|| {
            args.get("sinks").map_or_else(Vec::new, |s| {
                s.split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
        })
    })
}

// --- command builders (pure) ---------------------------------------------

pub fn clamp(percent: f64, ceiling: u32) -> u32 {
    percent.round().clamp(0.0, ceiling.min(MAX_VOLUME) as f64) as u32
}

fn argv<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.into_iter().map(str::to_owned).collect()
}

pub fn volume_command(sink: &str, percent: f64, ceiling: u32) -> Vec<String> {
    argv([
        "pactl",
        "set-sink-volume",
        sink,
        &format!("{}%", clamp(percent, ceiling)),
    ])
}

pub fn mute_command(sink: &str, muted: bool) -> Vec<String> {
    argv([
        "pactl",
        "set-sink-mute",
        sink,
        if muted { "1" } else { "0" },
    ])
}

pub fn default_command(sink: &str) -> Vec<String> {
    argv(["pactl", "set-default-sink", sink])
}

pub fn move_command(stream: u32, sink: &str) -> Vec<String> {
    argv(["pactl", "move-sink-input", &stream.to_string(), sink])
}

pub fn stream_volume_command(stream: u32, percent: f64) -> Vec<String> {
    argv([
        "pactl",
        "set-sink-input-volume",
        &stream.to_string(),
        &format!("{}%", clamp(percent, MAX_VOLUME)),
    ])
}

pub fn latency_offset_command(card: &str, port: &str, usec: i64) -> Vec<String> {
    argv([
        "pactl",
        "set-port-latency-offset",
        card,
        port,
        &usec.to_string(),
    ])
}

/// `module-combine-sink` load. PipeWire's pulse-compat argument parser splits on
/// whitespace with no quoting, so descriptions are hyphenated; `sink_name` must stay first
/// because module ownership is detected by that prefix.
pub fn combine_command<S: AsRef<str>>(sink: &str, description: &str, members: &[S]) -> Vec<String> {
    let joined = members
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "pactl".into(),
        "load-module".into(),
        "module-combine-sink".into(),
        format!("sink_name={sink}"),
        format!(
            "sink_properties=device.description={}",
            description.replace(' ', "-")
        ),
        format!("sinks={joined}"),
        "rate=48000".into(),
        "channels=2".into(),
        "latency_compensate=true".into(),
    ]
}

// --- execution ------------------------------------------------------------

/// Run an argv (`argv[0]` is the program) with a timeout; returns trimmed stdout.
pub async fn run(argv: &[String]) -> Result<String> {
    run_timeout(argv, TIMEOUT).await
}

pub async fn run_timeout(argv: &[String], timeout: Duration) -> Result<String> {
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("{program} is not installed"),
        Err(e) => bail!("{program}: {e}"),
    };
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => bail!("{} timed out", argv.join(" ")),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        match detail.trim().lines().last() {
            Some(line) if !line.trim().is_empty() => bail!("{}", line.trim()),
            _ => bail!("{program} failed"),
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Best-effort mutation: success flag instead of an error.
pub async fn try_run(argv: &[String]) -> bool {
    match run(argv).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("{}: {e:#}", argv.join(" "));
            false
        }
    }
}

async fn pactl(args: &[&str]) -> Result<String> {
    let mut v = Vec::with_capacity(args.len() + 1);
    v.push("pactl".to_owned());
    v.extend(args.iter().map(|s| (*s).to_owned()));
    run(&v).await
}

async fn pactl_json(args: &[&str]) -> Result<Value> {
    let out = pactl(args).await?;
    serde_json::from_str(&out).map_err(|e| anyhow!("pactl returned unexpected JSON: {e}"))
}

// --- live queries ---------------------------------------------------------

pub async fn server_info() -> Result<String> {
    run_timeout(&argv(["pactl", "info"]), Duration::from_secs(3)).await
}

pub async fn default_sink_name() -> Result<String> {
    pactl(&["get-default-sink"]).await
}

pub async fn default_or_empty() -> String {
    default_sink_name().await.unwrap_or_default()
}

/// Sinks with `is_default` computed against `default_sink` (pass `""` to skip).
pub async fn list_sinks(default_sink: &str) -> Result<Vec<Sink>> {
    Ok(parse_sinks(
        &pactl_json(&["--format=json", "list", "sinks"]).await?,
        default_sink,
    ))
}

pub async fn list_streams() -> Result<(Vec<Stream>, Vec<FanOut>)> {
    Ok(parse_streams(
        &pactl_json(&["--format=json", "list", "sink-inputs"]).await?,
    ))
}

pub async fn port_offsets() -> Result<HashMap<(String, String), i64>> {
    Ok(parse_port_offsets(
        &pactl_json(&["--format=json", "list", "cards"]).await?,
    ))
}

pub async fn modules() -> Result<String> {
    pactl(&["list", "short", "modules"]).await
}

pub async fn sink_exists(name: &str) -> Result<bool> {
    Ok(list_sinks("").await?.iter().any(|s| s.name == name))
}

pub async fn module_ids(sink: &str) -> Result<Vec<u32>> {
    Ok(parse_module_ids(&modules().await?, sink))
}

pub async fn combine_targets(sink: &str) -> Vec<String> {
    modules()
        .await
        .ok()
        .and_then(|m| parse_combine_targets(&m, sink))
        .unwrap_or_default()
}

/// Whether any fan-out stream of the combine sink `sink` still exists.
pub async fn has_fanout(sink: &str) -> Result<bool> {
    Ok(list_streams()
        .await?
        .1
        .iter()
        .any(|f| f.virtual_sink == sink))
}

/// Indices of application streams currently playing on the sink with `index`.
pub async fn streams_on(index: u32) -> Result<Vec<u32>> {
    Ok(list_streams()
        .await?
        .0
        .iter()
        .filter(|s| s.sink_index == index)
        .map(|s| s.index)
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub fn sinks_json() -> Value {
        json!([
            {"index": 40, "name": "alsa_output.usb-Generic_USB_Audio-01.analog-stereo", "description": "Generic USB Audio", "mute": false,
             "volume": {"front-left": {"value": 36045, "value_percent": "55%"}, "front-right": {"value": 36045, "value_percent": "55%"}},
             "properties": {"device.bus": "usb", "device.name": "alsa_card.usb-Generic_USB_Audio-01"}, "active_port": "analog-output", "state": "RUNNING"},
            {"index": 41, "name": "bluez_output.AA_BB_CC_DD_EE_FF.1", "description": "Bluetooth Headphones", "mute": true,
             "volume": {"front-left": {"value": 21627, "value_percent": "33%"}, "front-right": {"value": 21627, "value_percent": "33%"}},
             "properties": {"device.bus": "bluetooth"}},
            {"index": 42, "name": "alsa_output.pci-0000_01_00.1.hdmi-stereo", "description": "HDMI Digital Stereo", "mute": false,
             "volume": {"mono": {"value": 65536, "value_percent": "100%"}}, "properties": {"device.bus": "pci"}},
            {"index": 43, "name": "simultaneous_output", "description": "Simultaneous-Outputs", "mute": false,
             "volume": {"mono": {"value": 65536, "value_percent": "100%"}}, "properties": {"node.virtual": "true"}},
            {"index": 44, "name": "auto_null", "description": "Dummy Output", "mute": false,
             "volume": {"mono": {"value": 65536, "value_percent": "100%"}}, "properties": {}}
        ])
    }

    pub fn streams_json() -> Value {
        json!([
            {"index": 100, "sink": 43, "mute": false, "volume": {"mono": {"value": 65536, "value_percent": "100%"}},
             "properties": {"application.name": "Firefox", "application.process.binary": "firefox", "application.process.id": "4242", "media.name": "Playback"}},
            {"index": 101, "sink": 40, "mute": false, "volume": {"mono": {"value": 32768, "value_percent": "50%"}},
             "properties": {"media.name": "simultaneous_output output"}},
            {"index": 102, "sink": 41, "mute": false, "volume": {"mono": {"value": 32768, "value_percent": "50%"}},
             "properties": {"media.name": "simultaneous_route_chat_ab12 output"}},
            {"index": 103, "sink": 40, "mute": true, "volume": {"mono": {"value": 49152, "value_percent": "75%"}},
             "properties": {"application.name": "Example Chat", "application.process.binary": "example-chat", "media.name": "Voice call"}}
        ])
    }

    #[test]
    fn drops_dummy_output_and_orders_virtual_then_bluetooth() {
        let names: Vec<_> = parse_sinks(&sinks_json(), "")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "simultaneous_output",
                "bluez_output.AA_BB_CC_DD_EE_FF.1",
                "alsa_output.usb-Generic_USB_Audio-01.analog-stereo",
                "alsa_output.pci-0000_01_00.1.hdmi-stereo"
            ]
        );
    }

    #[test]
    fn classifies_every_connection_bucket() {
        let b: HashMap<_, _> = parse_sinks(&sinks_json(), "")
            .into_iter()
            .map(|s| (s.name, s.conn))
            .collect();
        assert_eq!(b["simultaneous_output"], Conn::Virtual);
        assert_eq!(b["bluez_output.AA_BB_CC_DD_EE_FF.1"], Conn::Bluetooth);
        assert_eq!(
            b["alsa_output.usb-Generic_USB_Audio-01.analog-stereo"],
            Conn::Usb
        );
        assert_eq!(b["alsa_output.pci-0000_01_00.1.hdmi-stereo"], Conn::Hdmi);
        assert_eq!(
            connection_of(
                &json!({"name": "alsa_output.pci-0000_00_1f.3.analog-stereo", "properties": {"device.bus": "pci"}})
            ),
            Conn::Analog
        );
    }

    #[test]
    fn averages_channel_volume_and_reads_mute_default_and_port() {
        let s: HashMap<_, _> = parse_sinks(&sinks_json(), "bluez_output.AA_BB_CC_DD_EE_FF.1")
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        let usb = &s["alsa_output.usb-Generic_USB_Audio-01.analog-stereo"];
        assert_eq!(usb.volume, 55);
        assert!(usb.is_running());
        assert_eq!(
            usb.port,
            Some((
                "alsa_card.usb-Generic_USB_Audio-01".into(),
                "analog-output".into()
            ))
        );
        assert!(s["bluez_output.AA_BB_CC_DD_EE_FF.1"].muted);
        assert!(s["bluez_output.AA_BB_CC_DD_EE_FF.1"].is_default);
        assert_eq!(
            volume_percent(&json!({"volume": {"a": {"value": 32768}, "b": {"value": 0.5}}})),
            50
        );
    }

    #[test]
    fn virtual_sink_label_is_rehumanised() {
        let primary = parse_sinks(&sinks_json(), "")
            .into_iter()
            .find(|s| s.is_primary())
            .unwrap();
        assert_eq!(primary.description, "Simultaneous-Outputs");
        assert_eq!(primary.label, "Simultaneous Outputs");
    }

    #[test]
    fn duplicate_descriptions_are_disambiguated_by_port() {
        let j = json!([
            {"index": 1, "name": "a.HiFi__Speaker__sink", "description": "USB Audio", "active_port": "[Out] Speaker",
             "ports": [{"name": "[Out] Speaker", "description": "Speaker"}], "properties": {}},
            {"index": 2, "name": "a.HiFi__SPDIF__sink", "description": "USB Audio", "active_port": "[Out] SPDIF",
             "ports": [{"name": "[Out] SPDIF", "description": "SPDIF"}], "properties": {}},
            {"index": 3, "name": "b", "description": "Example DAC", "properties": {}}
        ]);
        let labels: Vec<_> = parse_sinks(&j, "").into_iter().map(|s| s.label).collect();
        assert_eq!(
            labels,
            ["Example DAC", "USB Audio · SPDIF", "USB Audio · Speaker"]
        );
    }

    #[test]
    fn excludes_fanout_streams_of_primary_and_route_sinks() {
        let (streams, fanout) = parse_streams(&streams_json());
        assert_eq!(
            streams.iter().map(|s| s.index).collect::<Vec<_>>(),
            [100, 103]
        );
        assert_eq!(
            fanout,
            [
                FanOut {
                    virtual_sink: PRIMARY_SINK.into(),
                    member_index: 40
                },
                FanOut {
                    virtual_sink: "simultaneous_route_chat_ab12".into(),
                    member_index: 41
                }
            ]
        );
    }

    #[test]
    fn exposes_identity_used_by_routing_rules() {
        let firefox = &parse_streams(&streams_json()).0[0];
        assert_eq!(firefox.label(), "Firefox");
        assert_eq!(firefox.binary, "firefox");
        assert_eq!(firefox.pid, 4242);
        assert_eq!(firefox.detail(), "Playback");
    }

    #[test]
    fn volume_is_clamped_to_the_safe_range_and_ceiling() {
        assert_eq!(volume_command("sink", -5.0, 150).last().unwrap(), "0%");
        assert_eq!(volume_command("sink", 999.0, 150).last().unwrap(), "150%");
        assert_eq!(volume_command("sink", 140.0, 120).last().unwrap(), "120%");
    }

    #[test]
    fn mutations_are_argv_lists_that_no_shell_can_reparse() {
        assert_eq!(
            mute_command("a b", true),
            ["pactl", "set-sink-mute", "a b", "1"]
        );
        assert_eq!(default_command("a b"), ["pactl", "set-default-sink", "a b"]);
        assert_eq!(
            move_command(7, "sink"),
            ["pactl", "move-sink-input", "7", "sink"]
        );
        assert_eq!(
            stream_volume_command(7, 42.0),
            ["pactl", "set-sink-input-volume", "7", "42%"]
        );
        assert_eq!(
            latency_offset_command("card x", "[Out] SPDIF", -2500),
            [
                "pactl",
                "set-port-latency-offset",
                "card x",
                "[Out] SPDIF",
                "-2500"
            ]
        );
    }

    #[test]
    fn combine_command_hyphenates_description_and_joins_sinks() {
        let c = combine_command(PRIMARY_SINK, "Simultaneous Outputs", &["a", "b"]);
        assert!(!c[4].contains(' '));
        assert_eq!(c[3], "sink_name=simultaneous_output");
        assert_eq!(
            c[4],
            "sink_properties=device.description=Simultaneous-Outputs"
        );
        assert_eq!(c[5], "sinks=a,b");
    }

    #[test]
    fn module_listing_is_parsed_for_ownership_and_targets() {
        let m = "536870913\tmodule-combine-sink\tsink_name=simultaneous_output sink_properties=device.description=Simultaneous-Outputs sinks=a,b rate=48000\t\n\
                 536870914\tmodule-combine-sink\tsink_name=simultaneous_output_x sinks=c\t\n\
                 536870915\tmodule-null-sink\tsink_name=simultaneous_output\t\n\
                 536870916\tmodule-combine-sink\tsink_name=simultaneous_output\t";
        assert_eq!(parse_module_ids(m, PRIMARY_SINK), [536870913, 536870916]);
        assert_eq!(parse_combine_targets(m, PRIMARY_SINK).unwrap(), ["a", "b"]);
        assert_eq!(
            parse_combine_targets(m, "simultaneous_output_x").unwrap(),
            ["c"]
        );
        assert!(parse_combine_targets(m, "missing").is_none());
    }

    #[test]
    fn card_port_offsets_accept_pactl_text_and_numbers() {
        let j = json!([{"name": "card", "ports": {"analog-output": {"latency_offset": "50000 usec"}, "spdif": {"latency_offset": -20}}}]);
        let o = parse_port_offsets(&j);
        assert_eq!(o[&("card".into(), "analog-output".into())], 50000);
        assert_eq!(o[&("card".into(), "spdif".into())], -20);
    }
}
