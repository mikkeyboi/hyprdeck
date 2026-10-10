# External plugins

Hyprdeck plugins are **separate executables**, not native libraries or code built into Hyprdeck. The host has no device-specific logic. It provides a Plugins sidebar page, native GTK controls, an explicit trust/enable lifecycle, a headless CLI, and independent GitHub release installation and updates.

## Security and activation

A plugin is **unsandboxed user code**. Once enabled it can read and change your files, access your devices and network, and start other programs. A JSON interface is not a permission boundary. Review the source, executable and publisher before enabling. Never enable a plugin merely because its display name looks familiar.

- Installation copies/validates files but **does not run or enable the executable**.
- Checking releases fetches metadata/manifests, not executables, and does not run plugins.
- GUI **Enable and trust** displays a permissions confirmation. CLI `enable` is itself the explicit trust instruction and prints the permissions statement.
- Enable runs a `state` handshake. It saves the enabled id only after a successful, compatible response. A failed enable leaves activation unchanged.
- Disabled plugins cannot receive CLI state/action requests or GUI actions. Disabling stops future requests; an operation already started is allowed to finish under its deadline.
- Applying an update is explicit. An enabled plugin stays enabled and its new executable receives a state handshake. A failed handshake atomically restores the old directory. A disabled plugin stays disabled and **does not execute the new version**; its handshake is deferred to explicit enable.
- SHA256 protects against corrupt/mismatched downloads, **not a malicious publisher**. The checksum is downloaded from the same release and is not a signature. Updating an enabled plugin continues your trust in its existing repository.

Only install compatible Linux assets for the host architecture. The executable and manifest must be regular files, not symlinks; installation directories cannot be symlinks. Ids and filenames cannot contain separators or traversal. The host never runs an action through a shell or loads a shared library.

## Install, run, and manage

Open **Plugins** in the sidebar (`hyprdeck --page plugins`). Enter an existing local folder or `owner/repo`, click Install, then review the plugin and explicitly enable it. Each installed plugin shows its identity/version/repository, enable/disable, backend diagnostics, state groups/controls, and release check/update buttons. Failed manifests remain visible with a useful error instead of disappearing. Reload installed plugins picks up changes made by the CLI and discards pending edits.

Headless commands:

```sh
hyprdeck plugins list
hyprdeck plugins install ./hyprdeck-razer
hyprdeck plugins install mikkeyboi/hyprdeck-razer
hyprdeck plugins enable razer
hyprdeck plugins state razer
hyprdeck plugins action razer measure '{"device":"stable-id-from-state"}'
hyprdeck plugins check
hyprdeck plugins check razer
hyprdeck plugins update razer
hyprdeck plugins disable razer
```

`list` and `check` output JSON. `state` and successful `action` output the versioned response envelope described below; failures print an error and exit unsuccessfully. Action names, arguments and device ids come from the plugin's own state/documentation. `JSON args` must be an object and should be single-quoted in a shell. State and action require prior enable. `check` without an id attempts every installed plugin and fails if any check fails, while still printing successful results. There is no automatic action, hardware configuration, firmware flash, binary installation or update.

GitHub management requires `curl` and `sha256sum` on PATH (the same external-tool approach used by Hyprdeck's update features). Anonymous GitHub API rate limits apply; errors are explicit rather than silently treating stale data as a current release. Background checks use a one-hour cache, start after 30 seconds, and check hourly. Notifications use Hyprdeck's existing **System** notification policy: desktop/in-app/off, visibility, timing and error handling remain centralized. Notification deduplication is per plugin id and version. Notification failures are logged and retry on a later check. Manual checks force a fresh lookup; updates always re-fetch before replacement.

Installed layout:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/hyprdeck/plugins/<id>/
  plugin.json
  <executable>
${XDG_CONFIG_HOME:-$HOME/.config}/hyprdeck/plugins.toml
${XDG_STATE_HOME:-$HOME/.local/state}/hyprdeck/plugins/
  releases/<owner>_<repo>.json
  notified.json
```

For example `plugins.toml` contains `enabled = ["razer"]`. Missing settings mean no plugins enabled; malformed settings are an error, never silently replaced by defaults. Install clears a stale enabled id before adding a fresh plugin. Local installation copies only the manifest and executable: bundle resources in the executable rather than relying on extra files from the source folder. Local installation refuses to overwrite an existing plugin; normal replacement is through verified GitHub updates. To intentionally replace a local development plugin, disable it and remove **only its own installed directory**, then install/enable the new local folder.

## Manifest: `plugin.json`

All listed fields are required; unknown fields are accepted.

```json
{
  "api_version": 1,
  "id": "razer",
  "name": "Razer peripherals",
  "description": "OpenRazer controls and Wolverine diagnostics",
  "version": "0.1.0",
  "executable": "hyprdeck-razer",
  "update_repo": "mikkeyboi/hyprdeck-razer",
  "asset": "hyprdeck-razer-linux-x86_64"
}
```

- `api_version` must be integer `1` in both manifest and responses.
- `id`: at most 64 bytes; lowercase letter followed by lowercase ASCII letters, digits, `_` or `-`. Installed directory identity must match.
- `name` is nonempty; `description` can be empty. Displayed text is plain text, not Pango markup/HTML.
- `version`: stable semantic version `X.Y.Z`, no prerelease or build suffix. Comparisons are numeric, not lexical. Release tag must exactly match the version, optionally prefixed by `v`.
- `executable`: single filename, at most 200 bytes, starts with an ASCII alphanumeric, subsequent characters ASCII alphanumeric/`.`/`_`/`-`; cannot be `plugin.json`.
- `update_repo`: GitHub `owner/repo`, not a URL; each component follows the safe filename rules.
- `asset`: exactly `<executable>-linux-<Rust host architecture>`, e.g. `hyprdeck-razer-linux-x86_64` or `hyprdeck-demo-linux-aarch64`. Wrong-architecture assets are rejected before execution.

Names/descriptions and state texts are capped at 16,384 UTF-8 bytes and cannot contain NUL. The executable must have an executable permission bit and fit within 128 MiB. The JSON document limit is 1 MiB.

## Subprocess transport: API version 1

The host starts `<installed-executable> --request` directly, with the plugin directory as its current directory. It writes one JSON request to stdin and closes stdin; a newline is not required. The executable must read until EOF, emit exactly one JSON response on stdout and exit. Diagnostics belong on stderr. No banners, progress output or logging on stdout. Requests inherit the user's environment; do not depend on GTK or a graphical session for ordinary state/actions.

State request:

```json
{"api_version":1,"method":"state","action":null,"args":{}}
```

Action request:

```json
{"api_version":1,"method":"action","action":"set","args":{"device":"stable-id","setting":"lighting","value":true}}
```

Successful response, including after an action:

```json
{
  "api_version": 1,
  "error": null,
  "state": {
    "title": "Peripheral controls",
    "description": "Backend status and available capabilities",
    "groups": [
      {
        "id": "device-unique",
        "title": "Device",
        "description": "Connection and backend status",
        "rows": [
          {
            "id": "polling",
            "title": "USB polling",
            "subtitle": "250 Hz (descriptor, not measured)",
            "control": null
          }
        ]
      }
    ]
  }
}
```

Backend failure:

```json
{"api_version":1,"error":"Device disconnected; reconnect it and retry.","state":null}
```

A backend-reported error may exit successfully so the host can read its envelope; a nonzero process exit is also a failure with stderr diagnostics. `error` absent/null means success and requires a valid `state`. `state.description`, group description, row subtitle and row control may be absent; descriptions/subtitles default to empty and control defaults to null. Unknown object fields are accepted. Unsupported API versions, unknown control kinds, malformed JSON, invalid bounds, missing success state or duplicate ids fail clearly. The host retries live state after backend failures; actions are **never automatically retried**, because they may have side effects.

Each request has a **15-second** deadline. Stdout is bounded to 1 MiB and stderr to 64 KiB; excess output fails the request. Processes and their process-group descendants are terminated on timeout, cancellation and completion. Do not start persistent daemons or rely on a spawned GUI child remaining in the request's process group; use an existing desktop service/portal or a separate user-invoked CLI when appropriate. Long-running operations, interactive authentication and firmware flashing belong in an explicitly invoked standalone plugin command, not a host action that can be killed after 15 seconds.

State supports at most 128 groups and 1,024 total rows. Group ids must be unique in a state; row ids must be unique within a group. Keep ids stable across refreshes and hardware enumeration changes. Group/row titles and ids must be nonempty. Dynamic strings are displayed literally, including `<`, `>` and `&`.

### Peripheral visualization

API 1 also accepts optional `state.refresh_interval_ms` (250–10,000 ms, default 2,000),
`group.collapsed` (default false), and `group.visualization`. These are additive: older hosts
still show the group's ordinary rows. Always provide useful rows as a fallback.

The native controller card has front/rear views, live input highlights, stick positions and trigger
levels. Click a hotspot or use its keyboard-accessible selector to inspect an input. An optional
declarative `control` on that input provides real mapping/configuration UI; absence means inspection
only. The host never infers that a controller supports onboard remapping or RGB writes.

```json
{
  "id": "controller",
  "title": "Controller",
  "description": "Live input",
  "collapsed": false,
  "rows": [],
  "visualization": {
    "kind": "controller",
    "name": "Controller",
    "connection": "USB receiver",
    "status": "Sampled live input; onboard mapping unavailable",
    "inputs": [
      {"id":"a","label":"A","detail":"Standard input","pressed":false},
      {"id":"left_stick","label":"Left stick","detail":"Normalized position","x":0.0,"y":0.0},
      {"id":"lt","label":"Left trigger","detail":"Analog travel","value":0.0},
      {"id":"m3","label":"M3","detail":"Independent paddle state unavailable","pressed":null}
    ]
  }
}
```

Input ids are `a`, `b`, `x`, `y`, `lb`, `rb`, `lt`, `rt`, `left_stick`, `right_stick`,
`dpad_up`, `dpad_down`, `dpad_left`, `dpad_right`, `view`, `menu`, `guide`, and `m1`–`m6`.
Ids must be unique, with at most 32 inputs. `label` is nonempty; `detail` is plain text.
`pressed` is Boolean or null (unavailable), `value` is a finite 0–1 trigger level, and `x`/`y`
are finite −1–1 stick positions. Missing readings are unavailable, not released/centered claims.
`control` uses the existing native control schema below and must correspond to an actual backend
capability. Different peripheral layouts can be added as new validated visualization kinds without
bundling vendor backends. Arbitrary plugin SVG, HTML or executable UI scripts are not accepted.

Technical groups can set `collapsed: true` to appear under expandable details. Visualization values
refresh in-place, preserving selection and editing. Polling cadence is sampled telemetry, not a
USB packet-rate guarantee. Hiding the page stops refresh requests.

The host keeps plugin administration under **Manage plugins**, and each installed plugin's
activation/release controls under **plugin settings**. Controller cards appear first. The rear
layout is illustrative; displayed readings and mapping availability always come from the plugin.


## Native declarative controls

A row's `control` is null for information or one of the following objects. `action` is a nonempty backend-defined string (at most 128 bytes); `args` defaults to `{}`. The host interprets **no commands or device protocols**. The plugin must validate every action and argument against its real capabilities and report errors for missing devices, unsupported writes or permissions.

```json
{"kind":"button","label":"Measure input","action":"measure","args":{"device":"stable-id"},"destructive":false}
```

Native button; sends exactly its action/args on click. `destructive` defaults to false; true adds destructive styling and explicit confirmation.

```json
{"kind":"switch","value":false,"action":"set","args":{"device":"stable-id","setting":"lighting"}}
```

Native switch; after a user toggle the host inserts Boolean `args.value`. The backend's refreshed response determines the displayed value, not an optimistic write.

```json
{"kind":"number","value":500,"min":125,"max":1000,"step":125,"action":"set","args":{"device":"stable-id","setting":"polling"}}
```

Native spin control and Apply button. Values/bounds/step must be finite; `min <= value <= max` and `step > 0`. Apply commits a JSON number; typing/spinning is not a backend write.

```json
{"kind":"choice","value":"500","options":[{"value":"125","label":"125 Hz"},{"value":"500","label":"500 Hz"}],"action":"set","args":{"device":"stable-id","setting":"polling"}}
```

Native dropdown; user selection inserts string `args.value`. There must be 1–256 options, unique option values and a current value present in the options. Labels are plain text.

```json
{"kind":"text","value":"#00ff00","action":"set","args":{"device":"stable-id","setting":"color"}}
```

Native text entry and Apply button; Apply inserts string `args.value`. The backend owns validation (e.g. color syntax). Number/text Apply avoids accidental writes during rendering. `args.value` supplied by a control is overwritten with the user's selected value of the corresponding JSON type. Rendering or refreshing never sends an action.

The mapped Plugins page refreshes after completing its previous request, using the shortest enabled
plugin interval (default two seconds; validated 250–10,000 ms). Requests/actions remain serialized
across GUI and CLI through a file lock. Blocking work never runs on GTK's main thread. Same-shape
information and controller readings update existing widgets; pending edits and selected inputs are
preserved. Backend failures disable stale controls, show the error, and retry while mapped. Hiding
the page stops new refreshes; an already-started request may finish under its deadline.

## A complete minimal plugin

This standard-library Python example is a real, persistent settings plugin, useful for learning the API without a hardware SDK. It provides all five native control kinds and validates requests. It requires Python 3 at runtime; for a standalone release binary, use Rust as in the next section. The example's settings are not hardware claims.

Create a folder `demo-plugin` containing `plugin.json`:

```json
{
  "api_version": 1,
  "id": "demo",
  "name": "Demo preferences",
  "description": "A persistent example of all native controls",
  "version": "0.1.0",
  "executable": "hyprdeck-demo",
  "update_repo": "YOUR_OWNER/hyprdeck-demo",
  "asset": "hyprdeck-demo-linux-x86_64"
}
```

Replace `YOUR_OWNER` with your real GitHub account, and use the actual host architecture in `asset`. Save this as `demo-plugin/hyprdeck-demo`:

```python
#!/usr/bin/env python3
import json
import os
import pathlib
import sys
import tempfile

BASE = pathlib.Path(os.environ.get("XDG_CONFIG_HOME", pathlib.Path.home() / ".config"))
FILE = BASE / "hyprdeck-demo" / "settings.json"
DEFAULT = {"enabled": False, "level": 5, "mode": "quiet", "note": "Hello"}

def state(settings):
    def row(key, title, control):
        return {"id": key, "title": title, "subtitle": "Stored in your user configuration", "control": control}
    def control(kind, key, **fields):
        return {"kind": kind, "action": "set", "args": {"setting": key}, **fields}
    return {
        "title": "Demo preferences",
        "description": "Changes are persisted only after a user action.",
        "groups": [{"id": "preferences", "title": "Preferences", "description": "No hardware writes", "rows": [
            row("enabled", "Enabled", control("switch", "enabled", value=settings["enabled"])),
            row("level", "Level", control("number", "level", value=settings["level"], min=0, max=10, step=1)),
            row("mode", "Mode", control("choice", "mode", value=settings["mode"], options=[
                {"value": "quiet", "label": "Quiet"}, {"value": "verbose", "label": "Verbose"}])),
            row("note", "Note", control("text", "note", value=settings["note"])),
            row("reset", "Restore defaults", {"kind": "button", "label": "Reset", "action": "reset", "args": {}, "destructive": True})
        ]}]
    }

def save(settings):
    FILE.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=FILE.parent, prefix=".settings-")
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(settings, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, FILE)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)

def main():
    if sys.argv[1:] != ["--request"]:
        raise ValueError("Use --request with one JSON document on stdin")
    request = json.load(sys.stdin)
    if request.get("api_version") != 1 or request.get("method") not in ("state", "action"):
        raise ValueError("Unsupported API or method")
    settings = dict(DEFAULT)
    if FILE.exists():
        with FILE.open() as stream:
            settings.update(json.load(stream))
    if request["method"] == "action":
        args = request.get("args", {})
        if not isinstance(args, dict):
            raise ValueError("args must be an object")
        action = request.get("action")
        if action == "reset":
            settings = dict(DEFAULT)
        elif action == "set":
            key, value = args.get("setting"), args.get("value")
            valid = ((key == "enabled" and type(value) is bool)
                     or (key == "level" and type(value) in (int, float) and 0 <= value <= 10)
                     or (key == "mode" and value in ("quiet", "verbose"))
                     or (key == "note" and isinstance(value, str) and len(value.encode()) <= 16384 and "\x00" not in value))
            if not valid:
                raise ValueError("Invalid setting or value")
            settings[key] = value
        else:
            raise ValueError("Unknown action")
        save(settings)
    return {"api_version": 1, "error": None, "state": state(settings)}

try:
    response = main()
except Exception as error:
    response = {"api_version": 1, "error": str(error), "state": None}
json.dump(response, sys.stdout, allow_nan=False)
sys.stdout.write("\n")
```

Build/run/install the example:

```sh
chmod +x demo-plugin/hyprdeck-demo
printf '%s' '{"api_version":1,"method":"state","action":null,"args":{}}' | demo-plugin/hyprdeck-demo --request
hyprdeck plugins install ./demo-plugin
hyprdeck plugins enable demo
hyprdeck plugins state demo
hyprdeck plugins action demo set '{"setting":"note","value":"A persistent note"}'
hyprdeck --page plugins
```

## Build the independent Razer plugin

The separately maintained implementation is [mikkeyboi/hyprdeck-razer](https://github.com/mikkeyboi/hyprdeck-razer); follow its README for capabilities, OpenRazer setup, device permissions, controller limitations and standalone diagnostics. It is not part of Hyprdeck's host binary.

```sh
git clone https://github.com/mikkeyboi/hyprdeck-razer.git
cd hyprdeck-razer
cargo build --release --locked
# Assemble only the manifest and executable for local host installation.
mkdir -p dist
cp plugin.json dist/plugin.json
cp target/release/hyprdeck-razer dist/hyprdeck-razer
printf '%s' '{"api_version":1,"method":"state","action":null,"args":{}}' | dist/hyprdeck-razer --request
hyprdeck plugins install ./dist
hyprdeck plugins enable razer
hyprdeck plugins state razer
```

A state response may report absent OpenRazer/permissions while still exposing read-only diagnostics. That is a backend capability report, not permission to invent unsupported writes. Firmware discovery can be displayed through host controls, but flashing and other long-lived operations must use the plugin's documented standalone command with its own explicit confirmation.

## Publishing fixed release assets

For a stable Razer release `v0.1.0`, upload these **exact assets**:

```text
plugin.json
hyprdeck-razer-linux-x86_64
hyprdeck-razer-linux-x86_64.sha256
```

`plugin.json.executable` remains `hyprdeck-razer`; `asset` names the downloadable platform file. Hyprdeck installs it under the executable name and sets executable permissions. No archive extraction is involved. Replace names with your own executable for a different plugin, preserving the naming rule.

Example from the plugin source repository, with a manifest version already set to `0.1.0`:

```sh
cargo build --release --locked
mkdir -p release
cp plugin.json release/plugin.json
cp target/release/hyprdeck-razer release/hyprdeck-razer-linux-x86_64
(cd release && sha256sum hyprdeck-razer-linux-x86_64 > hyprdeck-razer-linux-x86_64.sha256)
gh release create v0.1.0 --title 'Razer plugin 0.1.0' --notes 'Describe capabilities, requirements, and changes.' \
  release/plugin.json release/hyprdeck-razer-linux-x86_64 release/hyprdeck-razer-linux-x86_64.sha256
```

For a real update, increment the manifest to a newer stable version and publish matching `vX.Y.Z` assets. Do not mutate an existing release to replace its executable. Build the asset for the named architecture and document runtime requirements; a wrong-format executable fails its enable/update handshake.

Checksums must contain one 64-character hexadecimal SHA256 digest, optionally followed by the **exact asset filename** (standard `sha256sum` output, with optional binary `*` prefix). Extra checksum lines or a different filename are rejected.

The host checks all of the following before replacement:

1. Stable/non-draft latest release; matching manifest version and release tag.
2. Supported API, safe paths and host-specific asset naming.
3. Manifest id, repository, executable and asset agree with the installed identity. A release cannot silently transfer trust to another repository or plugin.
4. New version is strictly newer. Equal/older releases cannot replace an installation.
5. Exactly one `plugin.json`, binary asset and checksum asset, with download URLs under the requested GitHub repository.
6. Download size matches metadata; binary SHA256 matches the checksum.
7. Complete manifest/executable staged on the same filesystem; Linux `renameat2(RENAME_EXCHANGE)` atomically swaps installed and staged directories. No missing-directory interval or partially copied binary is exposed to another host process.
8. Enabled runtime handshake succeeds, or the old directory is swapped back. Activation settings are not changed by update. If filesystem failure prevents rollback, the error names the retained backup path rather than deleting the only previous copy.

An interrupted staging/download leaves the previous installation intact. Cancellation during an active replacement restores the old directory via a guard. A forced process kill/power failure is not a runtime-handshake guarantee; inspect any `.stage-*` directory before manually deleting it, since an interrupted exchange may retain the old version there. An explicitly installed release is still trusted code; checksum and identity checks do not replace source review.

## Host integration and regression checks

The `hd-plugins` feature crate exports the same interfaces as existing features:

```rust
pub fn pages() -> Vec<hyprdeck_core::ui::PageInfo>;
pub fn start_background();
pub fn cli(args: &[String]) -> Option<anyhow::Result<()>>;
```

`pages()` contributes sidebar id `plugins`; no `PageInfo` API changes are needed. CLI dispatch takes the full feature argument list beginning with `plugins`.

Permanent regression coverage targets safe paths/symlink rejection, API/architecture/version/repository identity, exact checksum assets, bounded stdout/deadline handling, malformed response/backend error boundaries, atomic exchange failure, enabled handshake rollback retaining the old runnable manifest/binary, and disabled updates not executing a candidate. Run from the host workspace:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

For end-to-end validation use isolated `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `XDG_STATE_HOME`, an actual plugin executable/release, and the mapped Plugins page. Verify explicit trust, successful/failed actions, recoverable disconnects, no rendering writes, preserved edits, incompatible/mismatched/checksum-failed releases, enabled update rollback and disabled update nonexecution. Host regression tests do not claim hardware or firmware support; those belong to each independent backend.
