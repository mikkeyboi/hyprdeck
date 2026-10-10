//! External subprocess plugins. No device-specific code or native library loading.
mod backend;
mod controller;
mod illustration;
mod page;
mod process;
mod products;
mod protocol;
mod releases;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use hyprdeck_core::{rt, ui::PageInfo};
use serde_json::Map;

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "plugins",
        title: "Plugins",
        icon: "application-x-addon-symbolic",
        build: page::build,
    }]
}

/// Release metadata only: does not launch plugins, download binaries, or install updates.
pub fn start_background() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !STARTED.swap(true, Ordering::Relaxed) {
        rt::spawn(releases::background());
    }
}

const USAGE: &str = "usage: hyprdeck plugins list\n       hyprdeck plugins install <folder-or-owner/repo>\n       hyprdeck plugins enable <id>\n       hyprdeck plugins disable <id>\n       hyprdeck plugins state <id>\n       hyprdeck plugins action <id> <action> [JSON args]\n       hyprdeck plugins check [id]\n       hyprdeck plugins update <id>\n\nInstalling does not execute or enable a plugin. `enable` explicitly trusts unsandboxed code and runs its state handshake. Updates preserve activation and verify SHA256; enabled plugin updates run the new version's handshake. State/action require enable. JSON args must be an object. curl and sha256sum are required for GitHub releases.";

/// Existing feature dispatch convention: `None` unless the first token is plugins.
pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("plugins") {
        return None;
    }
    let rest: Vec<_> = args[1..].iter().map(String::as_str).collect();
    Some(rt::runtime().block_on(async {
        match rest.as_slice() {
            [] | ["list"] => {
                println!("{}", serde_json::to_string_pretty(&backend::list()?)?);
                Ok(())
            }
            ["install", source] => {
                let path = std::path::PathBuf::from(source);
                let manifest =
                    if path.is_dir() || source.starts_with('/') || source.starts_with('.') {
                        backend::install_local(path).await?
                    } else {
                        releases::install(source).await?
                    };
                println!(
                    "Installed {} {} (disabled; not executed).",
                    manifest.id, manifest.version
                );
                Ok(())
            }
            ["enable", id] => {
                eprintln!("{}", backend::TRUST);
                backend::enable(id).await?;
                println!("Enabled {id}; API handshake passed.");
                Ok(())
            }
            ["disable", id] => {
                backend::disable(id).await?;
                println!("Disabled {id}.");
                Ok(())
            }
            ["state", id] => print_state(backend::request(id, None, Map::new()).await?),
            ["action", id, action] => {
                print_state(backend::request(id, Some(action), Map::new()).await?)
            }
            ["action", id, action, args] => {
                let args = serde_json::from_str::<serde_json::Value>(args)?;
                let Some(args) = args.as_object() else {
                    bail!("JSON args must be an object");
                };
                print_state(backend::request(id, Some(action), args.clone()).await?)
            }
            ["check", id] => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&releases::check(id, true).await?)?
                );
                Ok(())
            }
            ["check"] => {
                let mut failed = false;
                let mut checks = Vec::new();
                for plugin in backend::list()? {
                    if let Some(error) = plugin.error {
                        eprintln!("{}: {error}", plugin.id);
                        failed = true;
                        continue;
                    }
                    match releases::check(&plugin.id, true).await {
                        Ok(check) => checks.push(check),
                        Err(error) => {
                            eprintln!("{}: {error:#}", plugin.id);
                            failed = true;
                        }
                    }
                }
                println!("{}", serde_json::to_string_pretty(&checks)?);
                if failed {
                    bail!("one or more plugin release checks failed");
                }
                Ok(())
            }
            ["update", id] => {
                let manifest = releases::update(id).await?;
                println!(
                    "Updated {} to {}; activation preserved.",
                    manifest.id, manifest.version
                );
                Ok(())
            }
            ["-h" | "--help" | "help"] => {
                println!("{USAGE}");
                Ok(())
            }
            _ => bail!("{USAGE}"),
        }
    }))
}

fn print_state(state: protocol::State) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&protocol::Response {
            api_version: protocol::API_VERSION,
            error: None,
            state: Some(state)
        })?
    );
    Ok(())
}
