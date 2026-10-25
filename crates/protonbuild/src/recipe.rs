//! Source-tree edits that turn a pristine GE-Proton checkout into a Hyprdeck build.
//!
//! Edits are anchored on stable lines rather than shipped as diffs, so one recipe keeps
//! working across GE releases. Every anchor must match exactly once; a tree that drifted
//! fails loudly instead of building with half the flags applied.
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

const MARKER: &str = "# hyprdeck-recipe";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagProfile {
    /// Upstream flags (`-O2 -march=nocona`).
    #[default]
    Stock,
    /// `-O3`; Wine's `-Werror` is dropped because `-O3` adds hundreds of warnings.
    O3,
    /// Proton-CachyOS style `x86-64-v3`: `-O3`, v3 scheduling and BMI/LZCNT/MOVBE, but no AVX.
    /// Windows-side code only gets a 16-byte aligned stack, so DXVK and d7vk refuse AVX.
    V3,
}

impl FlagProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stock => "Stock (-O2, nocona)",
            Self::O3 => "Optimized (-O3)",
            Self::V3 => "x86-64-v3 (-O3, no AVX)",
        }
    }

    /// Minimum x86-64 psABI level a build with these flags can run on.
    pub fn required_x86_64_level(self) -> u8 {
        match self {
            Self::Stock | Self::O3 => 1,
            Self::V3 => 3,
        }
    }
}

/// A download whose upstream URL disappeared; passed to make as `VARIABLE=value`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MakePin {
    pub variable: String,
    pub value: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// GE-Proton release tag, e.g. `GE-Proton11-1`.
    pub base: String,
    pub flags: FlagProfile,
    pub pins: Vec<MakePin>,
}

impl Recipe {
    pub fn new(base: &str, flags: FlagProfile) -> Self {
        Self {
            base: base.into(),
            flags,
            pins: known_pins(base),
        }
    }

    /// Steam-visible tool name, also used as the build and folder name.
    pub fn build_name(&self) -> String {
        let flags = match self.flags {
            FlagProfile::Stock => "stock",
            FlagProfile::O3 => "o3",
            FlagProfile::V3 => "v3",
        };
        format!("hyprdeck-{}-{flags}", self.base.to_ascii_lowercase())
    }

    /// Applies the recipe to a checkout that already ran `protonprep-valve-staging.sh`.
    /// Returns a human-readable list of the edits made.
    pub fn apply(&self, tree: &Path) -> Result<Vec<String>> {
        let mut edits = Vec::new();
        if self.flags == FlagProfile::Stock {
            return Ok(edits);
        }
        let makefile = tree.join("Makefile.in");
        let text = read(&makefile)?;
        ensure!(
            !text.contains(MARKER),
            "{} was already modified by a Hyprdeck recipe; start from a clean checkout",
            makefile.display()
        );
        let text = edit_makefile(&text, self.flags, &mut edits)?;
        fs::write(&makefile, text)?;

        if self.flags == FlagProfile::V3 {
            // Per-component, per-arch flags (OPENFST_i386_CFLAGS, ...) need a hook in the
            // generic rules; newer GE trees already carry it for meson.
            let common = tree.join("make/rules-common.mk");
            let text = edit_rules_common(&read(&common)?, &mut edits)?;
            fs::write(&common, text)?;
            let meson = tree.join("make/rules-meson.mk");
            let text = edit_rules_meson(&read(&meson)?, &mut edits)?;
            fs::write(&meson, text)?;
        }
        Ok(edits)
    }

    /// Extra `make` arguments: download pins.
    pub fn make_args(&self) -> Vec<String> {
        self.pins
            .iter()
            .map(|pin| format!("{}={}", pin.variable, pin.value))
            .collect()
    }
}

/// Whether `tag` is safe to pass to `git clone --branch`: a plain release tag name.
pub fn valid_base(tag: &str) -> bool {
    !tag.is_empty()
        && !tag.starts_with(['-', '.'])
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Pins for GE tags whose build downloads files that upstream mirrors have since removed.
pub fn known_pins(base: &str) -> Vec<MakePin> {
    let mut pins = Vec::new();
    if base == "GE-Proton11-1" {
        pins.push(MakePin {
            variable: "UNZIP_DEBIAN_REVISION".into(),
            value: "29+deb13u1".into(),
            reason: "Debian replaced unzip_6.0-29 with the 6.0-29+deb13u1 security update".into(),
        });
    }
    pins
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))
}

/// The single line index satisfying `matches`, or an error naming the anchor.
fn unique_line(lines: &[String], anchor: &str, matches: impl Fn(&str) -> bool) -> Result<usize> {
    let found: Vec<usize> = (0..lines.len()).filter(|&i| matches(&lines[i])).collect();
    match found.as_slice() {
        [index] => Ok(*index),
        [] => bail!("Recipe anchor not found: {anchor}"),
        _ => bail!(
            "Recipe anchor is ambiguous ({} matches): {anchor}",
            found.len()
        ),
    }
}

fn lines_of(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

fn join(lines: &[String], original: &str) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn edit_makefile(text: &str, flags: FlagProfile, edits: &mut Vec<String>) -> Result<String> {
    let mut lines = lines_of(text);

    let cflags = unique_line(&lines, "CFLAGS  = -O2", |l| {
        l.starts_with("CFLAGS") && !l.starts_with("CFLAGS +=") && l.contains("= -O2 ")
    })?;
    lines[cflags] = lines[cflags].replacen("-O2", "-O3", 1);
    edits.push("Makefile.in: CFLAGS -O2 → -O3".into());

    let werror = unique_line(&lines, "--enable-werror", |l| {
        l.trim() == "--enable-werror \\"
    })?;
    lines[werror] = lines[werror].replacen("--enable-werror", "--disable-werror", 1);
    edits.push("Makefile.in: Wine --enable-werror → --disable-werror".into());

    if flags == FlagProfile::V3 {
        for arch in ["i386_CFLAGS", "x86_64_CFLAGS"] {
            let index = unique_line(&lines, arch, |l| {
                l.starts_with(arch) && l.contains(":=") && l.contains("-march=nocona")
            })?;
            lines[index] = lines[index].replacen("-march=nocona", "-march=x86-64-v3", 1);
            edits.push(format!(
                "Makefile.in: {arch} -march=nocona → -march=x86-64-v3"
            ));
        }
        // Insert after the last `CFLAGS +=` line of the block that starts at `CFLAGS  =`.
        let mut end = cflags;
        while lines
            .get(end + 1)
            .is_some_and(|l| l.starts_with("CFLAGS +="))
        {
            end += 1;
        }
        let block = V3_BLOCK.lines().map(str::to_owned);
        lines.splice(end + 1..end + 1, block);
        edits.push(
            "Makefile.in: no AVX for Wine, -mno-bmi2 for openfst, -O3 for DXVK/vkd3d/dxvk-nvapi"
                .into(),
        );
    } else {
        lines.insert(cflags, format!("{MARKER}: -O3"));
    }
    Ok(join(&lines, text))
}

// Mirrors Proton-CachyOS cachyos_11.0_20261014/main for GCC builds.
const V3_BLOCK: &str = "
# hyprdeck-recipe: x86-64-v3 the Proton-CachyOS way (v3 scheduling and BMI, no AVX)
i386_CFLAGS   += -mno-avx -mno-avx2 -mno-avx512f -fvect-cost-model=cheap
x86_64_CFLAGS += -mno-avx -mno-avx2 -mno-avx512f -fvect-cost-model=cheap
i386_SANITY_FLAGS := -mpreferred-stack-boundary=2
OPENFST_i386_CFLAGS := -mno-bmi2
OPENFST_i386_CXXFLAGS := -mno-bmi2
OPENFST_x86_64_CFLAGS := -mno-bmi2
OPENFST_x86_64_CXXFLAGS := -mno-bmi2
DXVK_i386_CFLAGS = -O3 -mno-avx $(i386_SANITY_FLAGS)
DXVK_x86_64_CFLAGS = -O3 -mno-avx
DXVK_NVAPI_i386_CFLAGS = -O3 $(i386_SANITY_FLAGS)
DXVK_NVAPI_x86_64_CFLAGS = -O3
VKD3D_i386_CFLAGS = -O3 $(i386_SANITY_FLAGS)
VKD3D_x86_64_CFLAGS = -O3
VKD3D_PROTON_i386_CFLAGS = -O3 $(i386_SANITY_FLAGS)
VKD3D_PROTON_x86_64_CFLAGS = -O3
";

const ARCH_HOOK: &str = "$$($(2)_$(3)_CFLAGS)";

fn edit_rules_common(text: &str, edits: &mut Vec<String>) -> Result<String> {
    let mut lines = lines_of(text);
    for var in ["CFLAGS", "CPPFLAGS", "CXXFLAGS"] {
        let prefix = format!("{var}=\"$$($(2)_$(3)_INCFLAGS)");
        let index = unique_line(&lines, &prefix, |l| l.trim_start().starts_with(&prefix))?;
        if lines[index].contains(ARCH_HOOK) {
            continue;
        }
        let line = &lines[index];
        ensure!(
            line.ends_with("$$(CFLAGS)\" \\"),
            "Unexpected rules-common.mk {var} line: {line}"
        );
        lines[index] = line.replacen(
            "$$(CFLAGS)\" \\",
            &format!("$$(CFLAGS) {ARCH_HOOK}\" \\"),
            1,
        );
        edits.push(format!(
            "make/rules-common.mk: {var} reads per-arch component flags"
        ));
    }
    Ok(join(&lines, text))
}

fn edit_rules_meson(text: &str, edits: &mut Vec<String>) -> Result<String> {
    let mut lines = lines_of(text);
    for key in ["c_args", "cpp_args"] {
        let prefix = format!("{key} = [");
        let index = unique_line(&lines, &prefix, |l| l.starts_with(&prefix))?;
        if lines[index].contains(ARCH_HOOK) {
            continue;
        }
        let line = &lines[index];
        ensure!(
            line.ends_with("$$(CFLAGS))]"),
            "Unexpected rules-meson.mk {key} line: {line}"
        );
        lines[index] = line.replacen("$$(CFLAGS))]", &format!("{ARCH_HOOK} $$(CFLAGS))]"), 1);
        edits.push(format!(
            "make/rules-meson.mk: {key} reads per-arch component flags"
        ));
    }
    Ok(join(&lines, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed copies of the GE-Proton11-1 lines the recipe anchors on.
    const MAKEFILE: &str = "\
i386_CFLAGS    := -mstackrealign -march=nocona -mtune=core-avx2 -mfpmath=sse
x86_64_CFLAGS  := -mcmodel=small -march=nocona -mtune=core-avx2 -mfpmath=sse
aarch64_CFLAGS := -march=armv8.2-a -mtune=cortex-x3

CFLAGS  = -O2 -fwrapv -fno-strict-aliasing
CFLAGS += -ggdb -ffunction-sections -fdata-sections -fno-omit-frame-pointer
CFLAGS += -ffile-prefix-map=$(CCACHE_BASEDIR)=.

ifneq ($(SUPPRESS_WARNINGS),)
WINE_AUTOCONF_ARGS = \\
  --enable-werror \\
  --with-wayland \\
";
    const COMMON: &str = "\
    CFLAGS=\"$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS)\" \\
    CPPFLAGS=\"$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS)\" \\
    CXXFLAGS=\"$$($(2)_$(3)_INCFLAGS) -std=c++17 $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS)\" \\
    CROSSCFLAGS=\"$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS)\" \\
";
    const MESON: &str = "\
c_args = [$$(call list-quote,$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS))]
cpp_args = [$$(call list-quote,$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS))]
";

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("make")).unwrap();
        fs::write(dir.path().join("Makefile.in"), MAKEFILE).unwrap();
        fs::write(dir.path().join("make/rules-common.mk"), COMMON).unwrap();
        fs::write(dir.path().join("make/rules-meson.mk"), MESON).unwrap();
        dir
    }

    fn file(dir: &tempfile::TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(name)).unwrap()
    }

    #[test]
    fn stock_changes_nothing() {
        let dir = tree();
        let edits = Recipe::new("GE-Proton11-1", FlagProfile::Stock)
            .apply(dir.path())
            .unwrap();
        assert!(edits.is_empty());
        assert_eq!(file(&dir, "Makefile.in"), MAKEFILE);
    }

    #[test]
    fn o3_raises_optimization_and_drops_werror_only() {
        let dir = tree();
        Recipe::new("GE-Proton11-1", FlagProfile::O3)
            .apply(dir.path())
            .unwrap();
        let text = file(&dir, "Makefile.in");
        assert!(text.contains("CFLAGS  = -O3 -fwrapv"));
        assert!(text.contains("  --disable-werror \\"));
        assert!(text.contains("-march=nocona"));
        assert_eq!(file(&dir, "make/rules-common.mk"), COMMON);
    }

    #[test]
    fn v3_matches_cachyos_flags_and_adds_component_hooks() {
        let dir = tree();
        Recipe::new("GE-Proton11-1", FlagProfile::V3)
            .apply(dir.path())
            .unwrap();
        let text = file(&dir, "Makefile.in");
        assert!(!text.contains("-march=nocona"));
        assert_eq!(text.matches("-march=x86-64-v3").count(), 2);
        // The block lands after the CFLAGS += lines, before later definitions.
        let block = text.find("OPENFST_x86_64_CFLAGS := -mno-bmi2").unwrap();
        assert!(text.find("-ffile-prefix-map").unwrap() < block);
        assert!(block < text.find("ifneq").unwrap());
        let common = file(&dir, "make/rules-common.mk");
        assert_eq!(common.matches(ARCH_HOOK).count(), 3);
        assert!(common.contains(
            "CROSSCFLAGS=\"$$($(2)_$(3)_INCFLAGS) $$($(2)_CFLAGS) $$($(3)_CFLAGS) $$(CFLAGS)\""
        ));
        assert_eq!(
            file(&dir, "make/rules-meson.mk").matches(ARCH_HOOK).count(),
            2
        );
    }

    #[test]
    fn existing_meson_hook_is_kept_once() {
        let dir = tree();
        let hooked = MESON.replace("$$(CFLAGS))]", "$$($(2)_$(3)_CFLAGS) $$(CFLAGS))]");
        fs::write(dir.path().join("make/rules-meson.mk"), &hooked).unwrap();
        let edits = Recipe::new("GE-Proton11-1", FlagProfile::V3)
            .apply(dir.path())
            .unwrap();
        assert_eq!(file(&dir, "make/rules-meson.mk"), hooked);
        assert!(!edits.iter().any(|e| e.contains("rules-meson")));
    }

    #[test]
    fn second_application_and_drifted_trees_are_refused() {
        let dir = tree();
        let recipe = Recipe::new("GE-Proton11-1", FlagProfile::V3);
        recipe.apply(dir.path()).unwrap();
        assert!(recipe.apply(dir.path()).is_err());

        let dir = tree();
        fs::write(
            dir.path().join("Makefile.in"),
            MAKEFILE.replace("--enable-werror", "--enable-maintainer-mode"),
        )
        .unwrap();
        let err = Recipe::new("GE-Proton11-1", FlagProfile::O3)
            .apply(dir.path())
            .unwrap_err();
        assert!(err.to_string().contains("--enable-werror"));
    }

    #[test]
    fn pins_and_names_follow_the_base() {
        let recipe = Recipe::new("GE-Proton11-1", FlagProfile::V3);
        assert_eq!(recipe.build_name(), "hyprdeck-ge-proton11-1-v3");
        assert!(valid_base("GE-Proton11-1"));
        assert!(!valid_base("--upload-pack=x") && !valid_base("a b") && !valid_base(""));
        assert_eq!(recipe.make_args(), ["UNZIP_DEBIAN_REVISION=29+deb13u1"]);
        assert!(
            Recipe::new("GE-Proton11-11", FlagProfile::Stock)
                .pins
                .is_empty()
        );
    }
}
