//! Runtime compatibility for prebuilt runners, independent of Hyprdeck's build target.
use std::ffi::CStr;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuProfile {
    pub architecture: String,
    /// Highest complete, OS-usable x86-64 psABI level; 1 is baseline.
    /// Other architectures do not use the x86-64 levels.
    pub x86_64_level: Option<u8>,
}

impl CpuProfile {
    pub fn description(&self) -> String {
        match self.x86_64_level {
            Some(level) => format!("{} (usable x86-64-v{level})", self.architecture),
            None => self.architecture.clone(),
        }
    }
}

/// A stable snapshot of the running host, including OS-enabled vector state.
pub fn cpu_profile() -> CpuProfile {
    host_profile().clone()
}

pub(crate) fn host_profile() -> &'static CpuProfile {
    static PROFILE: OnceLock<CpuProfile> = OnceLock::new();
    PROFILE.get_or_init(|| {
        // SAFETY: uname fills this correctly sized structure and terminates machine.
        let mut host: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut host) } != 0 {
            return profile_from("unknown", Capabilities::default());
        }
        let machine = unsafe { CStr::from_ptr(host.machine.as_ptr()) }.to_string_lossy();
        let architecture = normalize_arch(&machine);
        let capabilities = if architecture == "x86_64" {
            detect_x86()
        } else {
            Capabilities::default()
        };
        profile_from(architecture, capabilities)
    })
}

fn normalize_arch(architecture: &str) -> &str {
    match architecture {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    }
}

// Raw feature snapshots make the psABI rules testable without consulting the host.
#[derive(Clone, Copy, Debug, Default)]
struct Capabilities {
    leaf1_ecx: u32,
    leaf7_ebx: u32,
    extended1_ecx: u32,
    xcr0: u64,
}

// SSE3, SSSE3, CX16, SSE4.1, SSE4.2, POPCNT; LAHF/SAHF is in the extended leaf.
const V2_LEAF1: u32 = (1 << 0) | (1 << 9) | (1 << 13) | (1 << 19) | (1 << 20) | (1 << 23);
const LAHF_SAHF: u32 = 1 << 0;
// FMA, MOVBE, XSAVE, OSXSAVE, AVX, F16C; OSXSAVE also gates XGETBV.
const V3_LEAF1: u32 = (1 << 12) | (1 << 22) | (1 << 26) | (1 << 27) | (1 << 28) | (1 << 29);
const V3_LEAF7: u32 = (1 << 3) | (1 << 5) | (1 << 8); // BMI1, AVX2, BMI2
const LZCNT: u32 = 1 << 5;
// AVX512F, AVX512DQ, AVX512CD, AVX512BW, AVX512VL.
const V4_LEAF7: u32 = (1 << 16) | (1 << 17) | (1 << 28) | (1 << 30) | (1 << 31);
const AVX_STATE: u64 = (1 << 1) | (1 << 2); // XMM, YMM
const AVX512_STATE: u64 = AVX_STATE | (1 << 5) | (1 << 6) | (1 << 7);

fn profile_from(architecture: &str, capabilities: Capabilities) -> CpuProfile {
    let architecture = normalize_arch(architecture);
    let x86_64_level = (architecture == "x86_64").then(|| {
        let c = capabilities;
        if c.leaf1_ecx & V2_LEAF1 != V2_LEAF1 || c.extended1_ecx & LAHF_SAHF == 0 {
            return 1;
        }
        if c.leaf1_ecx & V3_LEAF1 != V3_LEAF1
            || c.leaf7_ebx & V3_LEAF7 != V3_LEAF7
            || c.extended1_ecx & LZCNT == 0
            || c.xcr0 & AVX_STATE != AVX_STATE
        {
            return 2;
        }
        if c.leaf7_ebx & V4_LEAF7 != V4_LEAF7 || c.xcr0 & AVX512_STATE != AVX512_STATE {
            return 3;
        }
        4
    });
    CpuProfile {
        architecture: architecture.into(),
        x86_64_level,
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn detect_x86() -> Capabilities {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::{__cpuid, __cpuid_count, _xgetbv};
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::{__cpuid, __cpuid_count, _xgetbv};

    // SAFETY: CPUID is available on x86-64. Each optional leaf is range-checked.
    // XGETBV is executed only when both XSAVE and OSXSAVE advertise support.
    unsafe {
        let maximum = __cpuid(0).eax;
        if maximum < 1 {
            return Capabilities::default();
        }
        let leaf1_ecx = __cpuid(1).ecx;
        let leaf7_ebx = if maximum >= 7 {
            __cpuid_count(7, 0).ebx
        } else {
            0
        };
        let extended1_ecx = if __cpuid(0x8000_0000).eax >= 0x8000_0001 {
            __cpuid(0x8000_0001).ecx
        } else {
            0
        };
        let xsave = (1 << 26) | (1 << 27);
        let xcr0 = if leaf1_ecx & xsave == xsave {
            _xgetbv(0)
        } else {
            0
        };
        Capabilities {
            leaf1_ecx,
            leaf7_ebx,
            extended1_ecx,
            xcr0,
        }
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn detect_x86() -> Capabilities {
    // A non-x86 executable cannot query host CPUID (for example under emulation).
    // Architecture detection still works; optimized x86 tiers remain unproven.
    Capabilities::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_capabilities() -> Capabilities {
        Capabilities {
            leaf1_ecx: V2_LEAF1 | V3_LEAF1,
            leaf7_ebx: V3_LEAF7 | V4_LEAF7,
            extended1_ecx: LAHF_SAHF | LZCNT,
            xcr0: AVX512_STATE,
        }
    }

    #[test]
    fn baseline_and_each_complete_level_are_detected() {
        let mut c = Capabilities::default();
        assert_eq!(profile_from("x86_64", c).x86_64_level, Some(1));
        c.leaf1_ecx = V2_LEAF1;
        c.extended1_ecx = LAHF_SAHF;
        assert_eq!(profile_from("x86_64", c).x86_64_level, Some(2));
        c.leaf1_ecx |= V3_LEAF1;
        c.leaf7_ebx = V3_LEAF7;
        c.extended1_ecx |= LZCNT;
        c.xcr0 = AVX_STATE;
        assert_eq!(profile_from("x86_64", c).x86_64_level, Some(3));
        assert_eq!(
            profile_from("x86_64", full_capabilities()).x86_64_level,
            Some(4)
        );
    }

    #[test]
    fn every_required_instruction_feature_is_checked() {
        for bit in 0..32 {
            let mask = 1 << bit;
            if V2_LEAF1 & mask != 0 {
                let mut c = full_capabilities();
                c.leaf1_ecx &= !mask;
                assert_eq!(profile_from("x86_64", c).x86_64_level, Some(1));
            }
            if V3_LEAF1 & mask != 0 {
                let mut c = full_capabilities();
                c.leaf1_ecx &= !mask;
                assert_eq!(profile_from("x86_64", c).x86_64_level, Some(2));
            }
            if V3_LEAF7 & mask != 0 {
                let mut c = full_capabilities();
                c.leaf7_ebx &= !mask;
                assert_eq!(profile_from("x86_64", c).x86_64_level, Some(2));
            }
            if V4_LEAF7 & mask != 0 {
                let mut c = full_capabilities();
                c.leaf7_ebx &= !mask;
                assert_eq!(profile_from("x86_64", c).x86_64_level, Some(3));
            }
        }
        let mut c = full_capabilities();
        c.extended1_ecx &= !LAHF_SAHF;
        assert_eq!(profile_from("x86_64", c).x86_64_level, Some(1));
        c = full_capabilities();
        c.extended1_ecx &= !LZCNT;
        assert_eq!(profile_from("x86_64", c).x86_64_level, Some(2));
    }

    #[test]
    fn advertised_avx_requires_os_enabled_register_state() {
        for xcr0 in [0, 1 << 1, 1 << 2] {
            let c = Capabilities {
                xcr0,
                ..full_capabilities()
            };
            assert_eq!(profile_from("x86_64", c).x86_64_level, Some(2));
        }
        for bit in [5, 6, 7] {
            let c = Capabilities {
                xcr0: AVX512_STATE & !(1 << bit),
                ..full_capabilities()
            };
            assert_eq!(profile_from("x86_64", c).x86_64_level, Some(3));
        }
    }

    #[test]
    fn architecture_aliases_do_not_grant_x86_features_to_arm() {
        assert_eq!(
            profile_from("amd64", full_capabilities()).architecture,
            "x86_64"
        );
        let arm = profile_from("arm64", full_capabilities());
        assert_eq!(arm.architecture, "aarch64");
        assert_eq!(arm.x86_64_level, None);
        assert_eq!(
            profile_from("unknown", full_capabilities()).x86_64_level,
            None
        );
    }
}
