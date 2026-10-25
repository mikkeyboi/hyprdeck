//! Headless Hyprdeck Proton build, for CI releases and terminals without the app.
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hd_protonbuild::builder;
use hd_protonbuild::recipe::{self, FlagProfile, Recipe};

const USAGE: &str = "usage: hyprdeck-protonbuild <GE-Proton tag> <stock|o3|v3> --work <dir> [--jobs <n>] [--package <dir>]";

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("hyprdeck-protonbuild: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<()> {
    let [base, flags, rest @ ..] = args.as_slice() else {
        bail!(USAGE);
    };
    if !recipe::valid_base(base) {
        bail!("{base} is not a GE-Proton release tag");
    }
    let flags = match flags.as_str() {
        "stock" => FlagProfile::Stock,
        "o3" => FlagProfile::O3,
        "v3" => FlagProfile::V3,
        _ => bail!(USAGE),
    };
    let mut work = None;
    let mut package = None;
    let mut jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().context(USAGE)?;
        match flag.as_str() {
            "--work" => work = Some(PathBuf::from(value)),
            "--package" => package = Some(PathBuf::from(value)),
            "--jobs" => jobs = value.parse().context("--jobs takes a number")?,
            _ => bail!(USAGE),
        }
    }
    let work = work.context(USAGE)?;
    std::fs::create_dir_all(&work)?;
    let recipe = Recipe::new(base, flags);
    let plan = builder::plan(&recipe, &work, jobs);
    let started = Instant::now();
    builder::run(&plan, &AtomicBool::new(false), &mut |line| {
        println!("{line}")
    })?;
    println!(
        "==> Built {} in {}s",
        recipe.build_name(),
        started.elapsed().as_secs()
    );
    if let Some(dir) = package {
        let archive = builder::package(&plan, &dir)?;
        println!("==> Packed {}", archive.display());
    }
    Ok(())
}
