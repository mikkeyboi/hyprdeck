//! Run the headless commands without the app shell: `cargo run -p hd-audio --example audio_cli -- audio status`.
fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match hd_audio::cli(&args) {
        Some(Ok(())) => std::process::ExitCode::SUCCESS,
        Some(Err(e)) => {
            eprintln!("hyprdeck audio: {e:#}");
            std::process::ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: cli audio <command>");
            std::process::ExitCode::from(2)
        }
    }
}
