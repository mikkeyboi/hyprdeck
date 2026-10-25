# Game profiles and Hyprdeck Proton builds

Status: implemented, except launch health. The UI lives in two places under Gaming:

- **Compatibility tools** is the one inventory of runners. *Download tools* lists upstream forks and Hyprdeck
  Proton releases, *Installed tools* lists everything in `compatibilitytools.d` (downloads, local builds and
  variants, each with its lineage) and *Build* compiles Hyprdeck Proton locally.
- **Games** lists Steam games with their runner, Hyprdeck settings and prefix recovery. Global defaults sit at
  the top; a game overrides only the settings it needs.

Both are views of the same inventory, so a refresh, install or build in one shows up in the other.

Hyprdeck's Gaming section manages Proton in two layers. Most of what people want (HDR, DLSS, Reflex and
other low-latency options, ntsync) is a runtime setting that any modern Proton already understands, so it
belongs in a per-game profile that works with whichever runner the game uses. Only a few things need a new
build: compiler flags, the patch set and pinned component versions. Those go in an optional builder that
produces a Hyprdeck Proton locally, and the same recipe later runs in CI to publish releases.

## What needs a rebuild

| Feature | How it is controlled | Rebuild? |
| --- | --- | --- |
| HDR | `PROTON_ENABLE_HDR` (GE) or `DXVK_HDR` (Proton-CachyOS), plus Gamescope HDR | No |
| DLSS, DLSS upgrade, DLSS HUD | `PROTON_DLSS_UPGRADE`, `PROTON_DLSS_INDICATOR`, dxvk-nvapi env | No |
| FSR 4, XeSS upgrades | `PROTON_FSR4_UPGRADE`, `PROTON_XESS_UPGRADE` | No |
| Reflex / low latency | `DXVK_NVAPI_VKREFLEX`, `LOW_LATENCY_LAYER` | No |
| Low-latency DXVK / vkd3d forks | Swap the DLL set in the tool folder | No, overlay |
| ntsync, fsync | On by default; `PROTON_NO_NTSYNC` for troubleshooting | No |
| Native Wayland | `PROTON_ENABLE_WAYLAND` | No |
| Compiler flags (`-O3`, `x86-64-v3`) | Makefile flags | Yes |
| Patch set, component versions | Source tree | Yes |

## Measured build cost

GE-Proton11-1 built from source with 24 parallel jobs, ccache off, on an x86-64-v3 desktop:

| Step | Time | Size |
| --- | --- | --- |
| Clone with submodules | 5m23s | 7.2 GB |
| Steam Runtime SDK image (first build only) | download | 9.4 GB |
| Stock build | 13m53s | 504 MiB tarball |
| `-O3` build | 15m48s | 519 MiB tarball |
| `-O3` + `x86-64-v3` (CachyOS style, no AVX) | 16m14s | 519 MiB tarball |
| Rebuild after a small change | ~1m50s | |

Build folders take about 25 GB each. The rebuild floor is rsync and single-threaded gzip packaging, not
compiling, so switching packaging to zstd or installing straight to a staging folder will cut it further.

Findings that the builder has to handle:

- The only host requirements are Docker or Podman, git, make, rsync and network access. The toolchain
  (mingw, ccache, fontforge, afdko, nasm) lives in the SDK image.
- A plain `-march=x86-64-v3` build fails: DXVK and d7vk refuse AVX on purpose because Windows code only gets
  a 16-byte aligned stack, and openfst's 32-bit build breaks with BMI2. Proton-CachyOS builds v3 with
  `-mno-avx -mno-avx2 -mno-avx512f` added and `-mno-bmi2` for openfst, which leaves BMI1/2, LZCNT and MOVBE.
  GE has no per-component, per-arch flag hook, so the recipe also patches `make/rules-common.mk` and
  `make/rules-meson.mk` to read variables like `OPENFST_i386_CFLAGS`. Built this way the output has no AVX
  instructions, and BMI/LZCNT/MOVBE use rises sharply (DXVK's `d3d11.dll` from 223 to 2419 such instructions,
  Wine's `ntdll.dll` from 0 to 658). Whether that is measurable in games is still to be benchmarked.
- `-O3` adds hundreds of Wine warnings, so Wine's `--enable-werror` has to be off.
- Old GE tags download files that upstream mirrors later remove (for example Debian's `unzip_6.0-29`). The
  recipe pins replacement revisions, and CI should mirror these downloads.
- GE's `redist` target nests the previous output if run twice, and its xz autoreconf step fails on every other
  incremental run. The builder removes the output folder first and retries that step once.

## Per-game profiles

Settings live in `~/.config/hyprdeck/games/`: `defaults.json` holds the global defaults and `<appid>.json`
holds only the settings a game overrides. A game's effective settings are the defaults with its overrides on
top, so changing a default reaches every game that did not override it. Games shows them grouped by what
people care about:

- **Display**: HDR, native Wayland.
- **Upscaling**: DLSS upgrade and HUD, FSR 4 upgrade, XeSS upgrade.
- **Latency**: Reflex, low-latency layer, low-latency DXVK/vkd3d, frame rate limit.
- **Performance**: per-game shader cache.
- **Troubleshooting**: ntsync off, logging.

Each switch maps to the runner's own variable name, so one profile works on GE-Proton, Proton-CachyOS and
Hyprdeck builds. Settings a runner does not support are shown as unavailable rather than silently ignored.

### How a profile reaches the game

Hyprdeck creates a *variant* of a runner: a folder in `compatibilitytools.d` that hardlinks every file of the
runner (falling back to reflinks or copies across filesystems) and replaces only the `proton` entry point with
a small launcher. The original script stays in the variant as a hardlink named `proton.upstream`. Hyprdeck
writes each variant a `hyprdeck-games/defaults.json` (the defaults) and an `<appid>.json` per game with
overrides, already merged and translated for that runner. The launcher reads the game's file, or the
defaults when there is none, sets its variables and execs `proton.upstream`. Proton locates its files
from the directory of the script it was started as, so the run uses the variant's files. Swapped DLL sets, such as the low-latency DXVK fork, also live in the variant, so the
original runner is never modified and a new upstream release only needs its variant regenerated.

Variants are created from Installed tools. The dialog shows the lineage (the runner, and the recipe when the
runner is a Hyprdeck build), the settings the variant will apply and a progress bar while files are linked.
Each variant records that lineage in `.hyprdeck-variant.json`, so Installed tools can show it even after the
original runner is removed.

Assigning a game to a variant uses the existing compatibility-tool assignment flow, so it inherits the same
Steam-closed checks, backups and rollback. Steam launch options are left alone.

### Launch health

The launcher keeps a short record per game and runner from the Proton log. Two signatures are known: a log
that forwards `steam://run` back to Steam means a launch loop, and a failure to create `ROOT\WINE\WINEBUS`
means no controllers reach the game. Hyprdeck shows these next to the runner in the game's profile.

## Builder

The builder turns a recipe into an installed Hyprdeck Proton. A recipe names an upstream base (a GE-Proton
tag first), a flag profile (`stock`, `O3`, `v3`), optional component overrides and the pinned download fixes
above. It is the Build tab of Compatibility tools:

1. A requirements check for a container engine, git, make, rsync and free disk space, with an install button
   that uses the same pacman and polkit flow as the Updates page.
2. Recipe options and an estimate based on the timings above.
3. A progress view with the live build log, and install into `compatibilitytools.d` when it finishes. The
   install carries `.hyprdeck-build.json` with the recipe, which Installed tools shows as the build's lineage.

## CI releases

`.github/workflows/proton.yml` runs the same recipe through `hyprdeck-protonbuild`, a small command-line
front end in `crates/protonbuild` that needs no GTK. It is started by hand with a GE-Proton tag and a
revision, builds the `O3` and `v3` profiles in parallel and publishes a `proton-<tag>-<revision>` release in
this repository with `hyprdeck-<tag>-o3-x86_64.tar.gz` and `hyprdeck-<tag>-v3-x86_64_v3.tar.gz` plus
checksums. The release is never marked latest, because the app updater follows the latest release.

Download tools lists these under *Hyprdeck Proton* and picks the highest x86-64 level the CPU supports, the
same way it handles Proton-CachyOS. Each archive carries its recipe, so installed releases show their lineage
like local builds.

## Portability

Defaults favor CachyOS, but nothing depends on it: the builder needs only the container engine and basic
tools, and profiles depend only on the runner. Any Arch-based distribution running Hyprland is supported.
