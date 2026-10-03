//! `hyprdeck audio …` headless commands.

use anyhow::{Result, anyhow, bail};
use hyprdeck_core::rt;
use serde_json::json;

use crate::config::{MatchKind, Route};
use crate::{engine, pw};

const USAGE: &str = "\
Usage: hyprdeck audio <command>

  enable SINK SINK...     play on these outputs at the same time (becomes the default)
  off                     turn simultaneous output off (alias: disable)
  toggle                  on with the remembered group, or off
  status                  JSON status
  list-devices            connection<TAB>description<TAB>name per output
  list-streams            index<TAB>label<TAB>binary<TAB>media name per playing stream
  route MATCH SINK... [--match binary|app|media] [--label TEXT] [--volume PERCENT]
                          send streams containing MATCH to the outputs
  unroute MATCH|ID|LABEL  remove routing rules and send their streams back
  self-test               create and remove a temporary two-output combined sink";

pub fn run(args: &[String]) -> Result<()> {
    let Some(cmd) = args.first() else {
        println!("{USAGE}");
        return Ok(());
    };
    if matches!(cmd.as_str(), "help" | "--help" | "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    let rest = &args[1..];
    rt::runtime().block_on(async {
        if pw::server_info().await.is_err() {
            bail!("PipeWire/PulseAudio is not available in this session.");
        }
        match cmd.as_str() {
            "enable" => {
                if rest.len() < 2 {
                    bail!(
                        "enable needs at least two output names (see `hyprdeck audio list-devices`)"
                    );
                }
                println!("{}", engine::enable(rest).await?);
            }
            "off" | "disable" => println!("{}", engine::disable().await?),
            "toggle" => println!("{}", engine::toggle().await?),
            "status" => status().await?,
            "list-devices" => {
                for s in engine::status().await?.devices() {
                    println!("{}\t{}\t{}", s.conn.label(), s.description, s.name);
                }
            }
            "list-streams" => {
                for s in engine::status().await?.streams {
                    println!("{}\t{}\t{}\t{}", s.index, s.label(), s.binary, s.media_name);
                }
            }
            "route" => route(rest).await?,
            "unroute" => {
                let [key] = rest else {
                    bail!("usage: hyprdeck audio unroute MATCH|ID|LABEL")
                };
                let removed = engine::remove_cli_routes(key).await?;
                let names: Vec<&str> = removed.iter().map(Route::display).collect();
                println!("Removed {}", names.join(", "));
            }
            "self-test" => println!("{}", engine::self_test().await?),
            other => bail!("unknown audio command '{other}'\n\n{USAGE}"),
        }
        Ok(())
    })
}

async fn status() -> Result<()> {
    let st = engine::status().await?;
    let routes: Vec<_> = st
        .config
        .routes
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "label": r.display(),
                "match": {"kind": r.match_kind.as_str(), "value": r.match_value},
                "sinks": r.sinks,
                "enabled": r.enabled,
                "sink_name": r.sink_name(),
                "volume": r.volume,
            })
        })
        .collect();
    let out = json!({
        "active": st.active,
        "enabled": st.config.enabled,
        "default_sink": st.default_sink,
        "restore_sink": st.config.restore_sink,
        "selected_sinks": st.config.selected,
        "routes": routes,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Parsed `route` arguments.
#[derive(Debug, PartialEq)]
struct RouteArgs {
    value: String,
    sinks: Vec<String>,
    kind: MatchKind,
    label: Option<String>,
    volume: Option<u32>,
}

fn parse_route_args(args: &[String]) -> Result<RouteArgs> {
    let mut positional = Vec::new();
    let (mut kind, mut label, mut volume) = (MatchKind::Binary, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{flag} needs a value"))
        };
        match a.as_str() {
            "--match" => {
                let v = value("--match")?;
                kind = match v.as_str() {
                    "binary" | "app" | "media" => MatchKind::parse(&v),
                    _ => bail!("--match must be binary, app or media"),
                };
            }
            "--label" => label = Some(value("--label")?),
            "--volume" => {
                volume = Some(
                    value("--volume")?
                        .parse()
                        .map_err(|_| anyhow!("--volume needs a number"))?,
                )
            }
            _ => positional.push(a.clone()),
        }
    }
    if positional.len() < 2 {
        bail!(
            "usage: hyprdeck audio route MATCH SINK... [--match binary|app|media] [--label TEXT] [--volume PERCENT]"
        );
    }
    let value = positional.remove(0);
    Ok(RouteArgs {
        value,
        sinks: positional,
        kind,
        label,
        volume,
    })
}

async fn route(args: &[String]) -> Result<()> {
    let a = parse_route_args(args)?;
    let ignored: Vec<&String> = a.sinks.iter().filter(|s| pw::is_virtual_name(s)).collect();
    if !ignored.is_empty() {
        eprintln!(
            "Ignored virtual output(s): {}",
            ignored
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut route = Route {
        label: a.label.unwrap_or_else(|| a.value.clone()),
        match_kind: a.kind,
        match_value: a.value,
        sinks: a.sinks,
        volume: a.volume,
        ..Route::default()
    };
    route.normalize();
    if !route.valid() {
        bail!("A routing rule needs a match and at least one real output.");
    }
    let (route, moved) = engine::add_cli_route(route).await?;
    println!(
        "Routing '{}' to {} ({moved} stream(s) moved)",
        route.match_value,
        route.sinks.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn route_arguments_parse_flags_anywhere() {
        let a =
            parse_route_args(&args("player a b --match app --label Music --volume 70")).unwrap();
        assert_eq!(
            a,
            RouteArgs {
                value: "player".into(),
                sinks: vec!["a".into(), "b".into()],
                kind: MatchKind::App,
                label: Some("Music".into()),
                volume: Some(70)
            }
        );
        assert_eq!(
            parse_route_args(&args("--match media voice x"))
                .unwrap()
                .kind,
            MatchKind::Media
        );
        assert!(parse_route_args(&args("player")).is_err());
        assert!(parse_route_args(&args("player a --match neon")).is_err());
        assert!(parse_route_args(&args("player a --label")).is_err());
    }
}
