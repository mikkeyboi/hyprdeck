mod app;
mod prefs;
mod tray;

use std::process::ExitCode;

const USAGE: &str = "\
hyprdeck — desktop control center for Hyprland

USAGE:
  hyprdeck [--page <id>]          open the window (or focus the running instance)
  hyprdeck --background           login start: tray only if \"Start minimized\" is on
  hyprdeck --minimized            start (or keep) the tray without opening the window
  hyprdeck <command> …            headless commands:
      startup   list | enable <id> | disable <id>
      display   list | rescue | brightness <0-100> [output]
      input     binds | devices
      audio     enable | off | toggle | status | list-devices | list-streams | route … | unroute …
      bluetooth list | connect <addr|name> | disconnect <addr|name>
      defaults  list | set <category> <app.desktop>
      updates   check | json | apply [--yes] [--skip-aur]
                self [check | install [--channel stable|nightly]]
      system    diagnose | resume-log
      tweaks    gamemode on|off|toggle
      plugins   list | install <folder|owner/repo> | enable <id> | disable <id>
                state <id> | action <id> <action> [JSON args] | check [id] | update <id>

Pages: startup, display, input, keybinds, audio, bluetooth, defaults, updates, sleep, tweaks, plugins
       plugin:<id> opens an installed integration";

type Cli = fn(&[String]) -> Option<anyhow::Result<()>>;

fn main() -> ExitCode {
    // Logs go to stderr: stdout carries command output and the update helper's protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("HYPRDECK_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("-h" | "--help" | "help")
    ) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if matches!(args.first().map(String::as_str), Some("-V" | "--version")) {
        println!("hyprdeck {}", hyprdeck_core::version_string());
        return ExitCode::SUCCESS;
    }
    let clis: [Cli; 9] = [
        hd_startup::cli,
        hd_display::cli,
        hd_input::cli,
        hd_audio::cli,
        hd_bluetooth::cli,
        hd_defaults::cli,
        hd_updates::cli,
        hd_system::cli,
        hd_plugins::cli,
    ];
    if args.first().is_some_and(|a| !a.starts_with('-')) {
        // Headless commands are often piped (`hyprdeck display list | head`):
        // exit quietly on a closed pipe like other CLI tools instead of panicking.
        // SAFETY: called before any other thread exists; SIG_DFL is always valid.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        for cli in clis {
            if let Some(result) = cli(&args) {
                return match result {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("Error: {e:#}");
                        ExitCode::FAILURE
                    }
                };
            }
        }
        eprintln!("unknown command {:?}\n\n{USAGE}", args[0]);
        return ExitCode::from(2);
    }

    match app::run() {
        code if code == gtk::glib::ExitCode::SUCCESS => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
