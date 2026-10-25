//! Plans and runs a local Hyprdeck Proton build.
//!
//! Proton builds inside the Steam Runtime SDK container, so the host only needs a
//! container engine, git, make and rsync. The SDK image (about 9.4 GB) brings mingw,
//! ccache, fontforge and the rest of the toolchain.
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::recipe::{FlagProfile, Recipe};

pub const GE_REPO: &str = "https://github.com/GloriousEggroll/proton-ge-custom";

/// Source (~7.4 GB), one build tree (~26 GB), the SDK image (~9.4 GB) and headroom.
pub const REQUIRED_FREE_BYTES: u64 = 50 * 1024 * 1024 * 1024;

/// A failure this soon after `make` starts is GE's xz autoreconf flake, not a real error.
const QUICK_FAILURE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Requirement {
    pub name: &'static str,
    /// Arch package that provides it.
    pub package: &'static str,
    pub present: bool,
    pub detail: String,
}

/// Host checks for the requirements screen. `work_dir` is where sources and builds go.
pub fn requirements(work_dir: &Path) -> Vec<Requirement> {
    let engine = container_engine();
    let mut list = vec![Requirement {
        name: "Container engine",
        package: "podman",
        present: engine.is_some(),
        detail: match &engine {
            Some(engine) => format!("{engine} is installed and usable"),
            None => "Install Podman (rootless) or Docker; the build runs in the Steam Runtime SDK"
                .into(),
        },
    }];
    for (name, package) in [("git", "git"), ("make", "make"), ("rsync", "rsync")] {
        let present = on_path(name);
        list.push(Requirement {
            name,
            package,
            present,
            detail: if present {
                "Installed".into()
            } else {
                "Missing".into()
            },
        });
    }
    let free = free_bytes(existing_ancestor(work_dir));
    list.push(Requirement {
        name: "Free disk space",
        package: "",
        present: free.is_some_and(|free| free >= REQUIRED_FREE_BYTES),
        detail: match free {
            Some(free) => format!(
                "{} GB free, {} GB needed",
                free / 1_000_000_000,
                REQUIRED_FREE_BYTES / 1_000_000_000
            ),
            None => "Could not measure free space".into(),
        },
    });
    list
}

/// Arch packages to install for the missing requirements, for a single pacman prompt.
pub fn missing_packages(requirements: &[Requirement]) -> Vec<&'static str> {
    requirements
        .iter()
        .filter(|r| !r.present && !r.package.is_empty())
        .map(|r| r.package)
        .collect()
}

/// `podman` or `docker`, whichever answers `info`; Proton's configure.sh accepts either.
pub fn container_engine() -> Option<&'static str> {
    ["podman", "docker"].into_iter().find(|engine| {
        Command::new(engine)
            .arg("info")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
                && std::fs::metadata(&candidate).is_ok_and(|m| {
                    std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
                })
        })
    })
}

fn existing_ancestor(path: &Path) -> &Path {
    path.ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"))
}

fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct for a NUL-terminated path.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Rough cold-build estimate, scaled from measurements on 24 build jobs.
pub fn estimate(flags: FlagProfile, jobs: usize) -> Duration {
    let measured = match flags {
        FlagProfile::Stock => 13 * 60 + 53,
        FlagProfile::O3 => 15 * 60 + 48,
        FlagProfile::V3 => 16 * 60 + 14,
    };
    // Compilation scales with jobs; the ~2 minute packaging tail does not.
    let packaging = 110;
    let jobs = jobs.max(1) as u64;
    Duration::from_secs(packaging + (measured - packaging) * 24 / jobs)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Run {
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        /// Retry once when the command fails within [`QUICK_FAILURE`].
        retry_quick_failure: bool,
    },
    ApplyRecipe {
        tree: PathBuf,
    },
    /// Remove a previous redist output; GE nests the old one inside the new otherwise.
    RemoveDir(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub recipe: Recipe,
    pub source: PathBuf,
    pub build: PathBuf,
    pub steps: Vec<Step>,
}

impl Plan {
    /// Directory `make redist` leaves the finished tool in.
    pub fn output(&self) -> PathBuf {
        self.build.join(self.recipe.build_name())
    }
}

/// Plans a cold build of `recipe` under `work_dir/<base>`. Sources are cloned once per base
/// tag and kept pristine; each flag profile builds from its own copy of the tree.
pub fn plan(recipe: &Recipe, work_dir: &Path, jobs: usize) -> Plan {
    let name = recipe.build_name();
    let pristine = work_dir.join(&recipe.base).join("source");
    let source = work_dir.join(&recipe.base).join(format!("source-{name}"));
    let build = work_dir.join(&recipe.base).join(format!("build-{name}"));
    let run = |title: &str, program: &str, args: &[&str], cwd: &Path| Step {
        title: title.into(),
        action: Action::Run {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_owned(),
            retry_quick_failure: false,
        },
    };
    let mut steps = Vec::new();
    if !pristine.join(".git").exists() {
        steps.push(run(
            "Download GE-Proton source",
            "git",
            &[
                "clone",
                "--recurse-submodules",
                "--branch",
                &recipe.base,
                GE_REPO,
                &pristine.to_string_lossy(),
            ],
            work_dir,
        ));
        steps.push(run(
            "Apply GE patches",
            "sh",
            &[
                "-c",
                "./patches/protonprep-valve-staging.sh > patchlog.txt 2>&1",
            ],
            &pristine,
        ));
    }
    if !source.exists() {
        // A reflink-or-copy keeps the pristine tree reusable for other flag profiles.
        steps.push(run(
            "Copy source tree",
            "cp",
            &[
                "-a",
                "--reflink=auto",
                &pristine.to_string_lossy(),
                &source.to_string_lossy(),
            ],
            work_dir,
        ));
        steps.push(Step {
            title: format!("Apply {} flags", recipe.flags.label()),
            action: Action::ApplyRecipe {
                tree: source.clone(),
            },
        });
    }
    if !build.join("Makefile").exists() {
        steps.push(Step {
            title: "Create build directory".into(),
            action: Action::Run {
                program: "mkdir".into(),
                args: vec!["-p".into(), build.to_string_lossy().into_owned()],
                cwd: work_dir.to_owned(),
                retry_quick_failure: false,
            },
        });
        steps.push(run(
            "Configure (pulls the Steam Runtime SDK the first time)",
            &source.join("configure.sh").to_string_lossy(),
            &[&format!("--build-name={name}")],
            &build,
        ));
    }
    steps.push(Step {
        title: "Clear previous output".into(),
        action: Action::RemoveDir(build.join(&name)),
    });
    // Keep ccache and cargo state in the work dir instead of the user's ~/.ccache and ~/.cargo.
    let mut make = vec![
        format!("-j{}", jobs.max(1)),
        format!("CCACHE_DIR={}", work_dir.join("ccache").display()),
        format!("CARGO_HOME={}", work_dir.join("cargo-home").display()),
    ];
    make.extend(recipe.make_args());
    make.push("redist".into());
    steps.push(Step {
        title: "Build Proton".into(),
        action: Action::Run {
            program: "make".into(),
            args: make,
            cwd: build.clone(),
            retry_quick_failure: true,
        },
    });
    Plan {
        recipe: recipe.clone(),
        source,
        build,
        steps,
    }
}

/// Runs every step, sending each output line to `log`. Stops at the first failure.
pub fn run(plan: &Plan, cancel: &AtomicBool, log: &mut dyn FnMut(&str)) -> Result<()> {
    for step in &plan.steps {
        ensure!(!cancel.load(Ordering::Relaxed), "Build cancelled");
        log(&format!("==> {}", step.title));
        match &step.action {
            Action::ApplyRecipe { tree } => {
                for edit in plan.recipe.apply(tree)? {
                    log(&edit);
                }
            }
            Action::RemoveDir(dir) => {
                if dir.exists() {
                    std::fs::remove_dir_all(dir)
                        .with_context(|| format!("Removing {}", dir.display()))?;
                }
            }
            Action::Run {
                program,
                args,
                cwd,
                retry_quick_failure,
            } => {
                let started = Instant::now();
                let result = run_command(program, args, cwd, cancel, log);
                if result.is_err() && *retry_quick_failure && started.elapsed() < QUICK_FAILURE {
                    log("Failed early; retrying once (known GE xz autoreconf flake)");
                    run_command(program, args, cwd, cancel, log)?;
                } else {
                    result?;
                }
            }
        }
    }
    Ok(())
}

fn run_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    cancel: &AtomicBool,
    log: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Starting {program}"))?;
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let sender = sender.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(sender);
    loop {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => log(&line),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("Build cancelled");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait()?;
    ensure!(status.success(), "{program} failed ({status})");
    Ok(())
}

pub const BUILD_RECEIPT: &str = ".hyprdeck-build.json";

/// Copies a finished build into `tools_dir`, replacing an earlier Hyprdeck build of the
/// same name but never a folder Hyprdeck did not install.
pub fn install(plan: &Plan, tools_dir: &Path) -> Result<PathBuf> {
    let output = plan.output();
    ensure!(
        output.join("proton").is_file() && output.join("compatibilitytool.vdf").is_file(),
        "No finished build at {}",
        output.display()
    );
    let name = plan.recipe.build_name();
    let target = tools_dir.join(&name);
    if target.exists() {
        ensure!(
            target.join(BUILD_RECEIPT).is_file(),
            "{} exists and was not installed by Hyprdeck",
            target.display()
        );
    }
    std::fs::create_dir_all(tools_dir)?;
    let staging = tools_dir.join(format!(".{name}.staging"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let status = Command::new("cp")
        .args(["-a", "--reflink=auto"])
        .arg(&output)
        .arg(&staging)
        .status()
        .context("Starting cp")?;
    ensure!(status.success(), "Copying the build failed ({status})");
    std::fs::write(
        staging.join(BUILD_RECEIPT),
        serde_json::to_string_pretty(&plan.recipe)? + "\n",
    )?;
    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    std::fs::rename(&staging, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_plan_clones_once_and_retries_only_make() {
        let work = tempfile::tempdir().unwrap();
        let recipe = Recipe::new("GE-Proton11-1", FlagProfile::V3);
        let plan = plan(&recipe, work.path(), 24);
        let titles: Vec<_> = plan.steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles[0], "Download GE-Proton source");
        assert!(titles.contains(&"Apply x86-64-v3 (-O3, no AVX) flags"));
        let Action::Run {
            args,
            retry_quick_failure,
            ..
        } = &plan.steps.last().unwrap().action
        else {
            panic!("make is last");
        };
        assert!(retry_quick_failure);
        assert_eq!(args[0], "-j24");
        assert_eq!(
            args[1],
            format!("CCACHE_DIR={}", work.path().join("ccache").display())
        );
        assert_eq!(&args[3..], ["UNZIP_DEBIAN_REVISION=29+deb13u1", "redist"]);
        assert_eq!(
            plan.output(),
            work.path()
                .join("GE-Proton11-1/build-hyprdeck-ge-proton11-1-v3/hyprdeck-ge-proton11-1-v3")
        );
    }

    #[test]
    fn rebuild_plan_skips_clone_and_configure_but_clears_output() {
        let work = tempfile::tempdir().unwrap();
        let recipe = Recipe::new("GE-Proton11-1", FlagProfile::Stock);
        let base = work.path().join("GE-Proton11-1");
        std::fs::create_dir_all(base.join("source/.git")).unwrap();
        std::fs::create_dir_all(base.join("source-hyprdeck-ge-proton11-1-stock")).unwrap();
        let build = base.join("build-hyprdeck-ge-proton11-1-stock");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join("Makefile"), "").unwrap();
        let plan = plan(&recipe, work.path(), 8);
        let titles: Vec<_> = plan.steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Clear previous output", "Build Proton"]);
    }

    #[test]
    fn estimates_scale_compile_time_but_not_packaging() {
        assert_eq!(estimate(FlagProfile::Stock, 24).as_secs(), 13 * 60 + 53);
        // Three times fewer jobs triples compilation, but packaging stays ~110 s.
        assert_eq!(
            estimate(FlagProfile::Stock, 8).as_secs(),
            110 + (13 * 60 + 53 - 110) * 3
        );
        assert!(estimate(FlagProfile::V3, 24) > estimate(FlagProfile::O3, 24));
    }

    #[test]
    fn early_failures_are_retried_once_and_logged() {
        let work = tempfile::tempdir().unwrap();
        let marker = work.path().join("ran-once");
        let script = format!(
            "if [ -e {0} ]; then echo second; else touch {0}; echo first; exit 2; fi",
            marker.display()
        );
        let plan = Plan {
            recipe: Recipe::new("GE-Proton11-1", FlagProfile::Stock),
            source: work.path().into(),
            build: work.path().into(),
            steps: vec![Step {
                title: "flaky".into(),
                action: Action::Run {
                    program: "sh".into(),
                    args: vec!["-c".into(), script],
                    cwd: work.path().into(),
                    retry_quick_failure: true,
                },
            }],
        };
        let mut lines = Vec::new();
        run(&plan, &AtomicBool::new(false), &mut |l| {
            lines.push(l.to_owned())
        })
        .unwrap();
        assert_eq!(lines.first().unwrap(), "==> flaky");
        assert!(lines.contains(&"first".to_owned()) && lines.contains(&"second".to_owned()));
    }

    #[test]
    fn install_copies_output_and_refuses_foreign_folders() {
        let work = tempfile::tempdir().unwrap();
        let tools = work.path().join("compatibilitytools.d");
        let recipe = Recipe::new("GE-Proton11-1", FlagProfile::O3);
        let plan = plan(&recipe, work.path(), 4);
        let output = plan.output();
        std::fs::create_dir_all(output.join("files")).unwrap();
        std::fs::write(output.join("proton"), "script").unwrap();
        std::fs::write(output.join("compatibilitytool.vdf"), "vdf").unwrap();
        let installed = install(&plan, &tools).unwrap();
        assert_eq!(installed, tools.join("hyprdeck-ge-proton11-1-o3"));
        assert!(installed.join(BUILD_RECEIPT).is_file());
        // Reinstalling replaces Hyprdeck's own build.
        install(&plan, &tools).unwrap();
        std::fs::remove_file(installed.join(BUILD_RECEIPT)).unwrap();
        assert!(install(&plan, &tools).is_err());
    }

    #[test]
    fn missing_requirements_map_to_packages() {
        let reqs = vec![
            Requirement {
                name: "Container engine",
                package: "podman",
                present: false,
                detail: String::new(),
            },
            Requirement {
                name: "git",
                package: "git",
                present: true,
                detail: String::new(),
            },
            Requirement {
                name: "Free disk space",
                package: "",
                present: false,
                detail: String::new(),
            },
        ];
        assert_eq!(missing_packages(&reqs), ["podman"]);
    }
}
