// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SPIKE (NVX time ABI v1, not for integration): environment-selected
//! restored-TSC synchronization modes and the ABI v1 guest time identity on
//! the WHP microVM path.
//!
//! - `NVX_SPIKE_WHP_TSC=emulate|native|native-resume` selects how restored
//!   SMP partitions keep their TSCs synchronized.
//! - `NVX_SPIKE_TIME_ABI=1` serves the minimal Hyper-V frequency and
//!   invariant-TSC identity from OpenVMM, pins the CPUID time bits, removes
//!   the fixed 1 GHz TSC request, and pins the offloaded APIC at 200 MHz.

use crate::Error;
use crate::WhpResultExt;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use virt::x86::MsrError;
use x86defs::cpuid::CpuidFunction;

/// Restore-time TSC synchronization mode of SMP partitions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TscMode {
    /// Align with partition time suspended, then emulate RDTSC and the TSC
    /// MSRs for the VM's lifetime (the baseline).
    Emulate,
    /// Align with partition time suspended; the first VP run resumes time.
    Native,
    /// Align with partition time suspended, then resume time explicitly
    /// before any VP runs.
    NativeResume,
}

pub(crate) fn tsc_mode() -> TscMode {
    static MODE: OnceLock<TscMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = match std::env::var("NVX_SPIKE_WHP_TSC").as_deref() {
            Ok("native") => TscMode::Native,
            Ok("native-resume") => TscMode::NativeResume,
            Ok("emulate") | Err(_) => TscMode::Emulate,
            Ok(other) => panic!("unknown NVX_SPIKE_WHP_TSC value {other:?}"),
        };
        tracing::info!(?mode, "spike: WHP restored TSC mode");
        mode
    })
}

/// Whether the ABI v1 guest time identity is enabled.
pub(crate) fn identity_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("NVX_SPIKE_TIME_ABI").as_deref() == Ok("1"))
}

/// Reads every VP's frozen TSC and logs whether they all equal `expected`.
pub(crate) fn check_frozen_tscs(partition: &whp::Partition, vp_count: usize, expected: u64) {
    let values: Vec<u64> = (0..vp_count as u32)
        .map(|vp| {
            partition
                .vp(vp)
                .get_register(whp::Register64::Tsc)
                .unwrap_or(u64::MAX)
        })
        .collect();
    let equal = values.iter().all(|&value| value == expected);
    tracing::info!(
        equal,
        expected,
        ?values,
        mode = ?tsc_mode(),
        "spike: frozen restored TSC read-back"
    );
}

/// The offloaded APIC timer rate of time ABI v1 on WHP.
pub(crate) const APIC_FREQUENCY_HZ: u64 = 200_000_000;

const MSR_VP_INDEX: u32 = 0x4000_0002;
const MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
const MSR_APIC_FREQUENCY: u32 = 0x4000_0023;
const MSR_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;

/// HYPERCALL_AVAILABLE (detection only), VP_INDEX_AVAILABLE,
/// ACCESS_FREQUENCY_MSRS, and ACCESS_TSC_INVARIANT.
const PRIVILEGES_LOW: u32 = (1 << 5) | (1 << 6) | (1 << 11) | (1 << 15);
/// FREQUENCY_MSRS_AVAILABLE.
const FEATURES_EDX: u32 = 1 << 8;
/// Placeholder NVX build and version for leaf 0x40000002 (spec decision).
const NVX_BUILD: u32 = 1;
const NVX_VERSION: u32 = 0x0001_0000;

const LEAF_POWER_MANAGEMENT: u32 = 0x6;
const LEAF_PERFORMANCE_MONITORING: u32 = 0xa;
const LEAF_PROCESSOR_FREQUENCY: u32 = 0x16;
const LEAF_ADVANCED_POWER_MANAGEMENT: u32 = 0x8000_0007;

/// The partition's TSC_INVARIANT_CONTROL value. The spike runs one partition
/// per process; the real implementation keeps this in saved partition state.
static TSC_INVARIANT_CONTROL: AtomicU64 = AtomicU64::new(0);

/// The CPUID leaves of the time identity: exactly the identity table in
/// 0x40000000..=0x400000ff, plus the time bits of the CPU profile.
pub(crate) fn identity_cpuid_leaves(vp_capacity: u32) -> Vec<virt::CpuidLeaf> {
    let mut leaves: Vec<virt::CpuidLeaf> = (0x4000_0000..=0x4000_00ffu32)
        .map(|function| {
            let result = match function {
                0x4000_0000 => [
                    0x4000_0005,
                    u32::from_le_bytes(*b"Micr"),
                    u32::from_le_bytes(*b"osof"),
                    u32::from_le_bytes(*b"t Hv"),
                ],
                0x4000_0001 => [u32::from_le_bytes(*b"Hv#1"), 0, 0, 0],
                0x4000_0002 => [NVX_BUILD, NVX_VERSION, 0, 0],
                0x4000_0003 => [PRIVILEGES_LOW, 0, 0, FEATURES_EDX],
                0x4000_0004 => [0, 0xffff_ffff, 0, 0],
                0x4000_0005 => [vp_capacity, vp_capacity, 0, 0],
                _ => [0; 4],
            };
            virt::CpuidLeaf::new(function, result)
        })
        .collect();

    // Hypervisor present; no TSC-deadline timer.
    leaves.push(
        virt::CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, 1 << 31, 0]).masked([
            0,
            0,
            (1 << 31) | (1 << 24),
            0,
        ]),
    );
    // ARAT; no HWP (EAX[7..=11]) and no APERF/MPERF (ECX[0]).
    leaves.push(
        virt::CpuidLeaf::new(LEAF_POWER_MANAGEMENT, [1 << 2, 0, 0, 0]).masked([
            (1 << 2) | (0x1f << 7),
            0,
            1,
            0,
        ]),
    );
    // No TSC_ADJUST.
    leaves.push(
        virt::CpuidLeaf::new(CpuidFunction::ExtendedFeatures.0, [0; 4])
            .indexed(0)
            .masked([0, 1 << 1, 0, 0]),
    );
    // No PMU, and no processor frequency leaf.
    leaves.push(virt::CpuidLeaf::new(LEAF_PERFORMANCE_MONITORING, [0; 4]));
    leaves.push(virt::CpuidLeaf::new(LEAF_PROCESSOR_FREQUENCY, [0; 4]));
    // Invariant TSC.
    leaves.push(
        virt::CpuidLeaf::new(LEAF_ADVANCED_POWER_MANAGEMENT, [0, 0, 0, 1 << 8]).masked([
            0,
            0,
            0,
            1 << 8,
        ]),
    );
    leaves
}

/// Configures a time-ABI partition: CPUID exits for the topology, clock,
/// identity, and time-bit leaves; time feature banks; unhandled-MSR exits; and
/// the 200 MHz APIC. Unlike the versioned contract, the TSC runs at the
/// host-native rate.
pub(crate) fn configure_identity_partition(
    whp_config: &mut whp::PartitionConfig,
    extended_exits: &mut whp::abi::WHV_EXTENDED_VM_EXITS,
) -> Result<(), Error> {
    *extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit;
    let mut exit_list = vec![
        CpuidFunction::VendorAndMaxFunction.0,
        CpuidFunction::VersionAndFeatures.0,
        CpuidFunction::CacheParameters.0,
        LEAF_POWER_MANAGEMENT,
        CpuidFunction::ExtendedFeatures.0,
        LEAF_PERFORMANCE_MONITORING,
        CpuidFunction::ExtendedTopologyEnumeration.0,
        CpuidFunction::CoreCrystalClockInformation.0,
        LEAF_PROCESSOR_FREQUENCY,
        CpuidFunction::V2ExtendedTopologyEnumeration.0,
        LEAF_ADVANCED_POWER_MANAGEMENT,
        CpuidFunction::ExtendedAddressSpaceSizes.0,
        CpuidFunction::ProcessorTopologyDefinition.0,
    ];
    exit_list.extend(0x4000_0000..=0x4000_00ff);
    whp_config
        .set_property(whp::PartitionProperty::CpuidExitList(&exit_list))
        .for_op("set time ABI CPUID exits")?;

    let capability = whp::capabilities::processor_features().for_op("get processor features")?;
    let tsc_invariant = whp::abi::WHV_PROCESSOR_FEATURES1(1 << 1);
    let mut features = capability;
    features.bank1 &= !(whp::abi::WHV_PROCESSOR_FEATURES1::TscDeadlineTmrSupport
        | whp::abi::WHV_PROCESSOR_FEATURES1::TscAdjustSupport
        | whp::abi::WHV_PROCESSOR_FEATURES1::ACountMCountSupport);
    let result = whp_config
        .set_property(whp::PartitionProperty::ProcessorFeatures(features))
        .map(drop);
    tracing::info!(
        bank0 = format_args!("{:#x}", capability.bank0.0),
        bank1 = format_args!("{:#x}", capability.bank1.0),
        partition_bank1 = format_args!("{:#x}", features.bank1.0),
        tsc_invariant_capability = capability.bank1.is_set(tsc_invariant),
        tsc_deadline_capability = capability
            .bank1
            .is_set(whp::abi::WHV_PROCESSOR_FEATURES1::TscDeadlineTmrSupport),
        tsc_adjust_capability = capability
            .bank1
            .is_set(whp::abi::WHV_PROCESSOR_FEATURES1::TscAdjustSupport),
        ?result,
        "spike: time ABI processor feature banks"
    );
    result.for_op("set time ABI processor features")?;

    let msr_capability =
        whp::capabilities::x64_msr_exit_bitmap().for_op("get MSR exit capability")?;
    tracing::info!(
        capability = format_args!("{:#x}", msr_capability.0),
        "spike: time ABI MSR exit capability"
    );
    whp_config
        .set_property(whp::PartitionProperty::X64MsrExitBitmap(
            whp::abi::WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs,
        ))
        .for_op("set time ABI MSR exits")?;

    let result = whp_config
        .set_property(whp::PartitionProperty::InterruptClockFrequency(
            APIC_FREQUENCY_HZ,
        ))
        .map(drop);
    tracing::info!(
        capability = ?whp::capabilities::interrupt_clock_frequency(),
        ?result,
        "spike: time ABI APIC frequency pin"
    );
    Ok(())
}

/// Reads an identity MSR. `None` leaves the MSR to the existing handlers.
pub(crate) fn identity_msr_read(
    msr: u32,
    vp_index: u32,
    tsc_frequency_hz: u64,
) -> Option<Result<u64, MsrError>> {
    if !identity_enabled() || !(0x4000_0000..=0x4000_01ff).contains(&msr) {
        return None;
    }
    let result = match msr {
        MSR_VP_INDEX => Ok(vp_index.into()),
        MSR_TSC_FREQUENCY => Ok(tsc_frequency_hz),
        MSR_APIC_FREQUENCY => Ok(APIC_FREQUENCY_HZ),
        MSR_TSC_INVARIANT_CONTROL => Ok(TSC_INVARIANT_CONTROL.load(Ordering::Relaxed)),
        _ => Err(MsrError::Unknown),
    };
    tracing::debug!(
        vp_index,
        msr = format_args!("{msr:#x}"),
        ?result,
        "spike: identity MSR read"
    );
    Some(result)
}

/// Writes an identity MSR. `None` leaves the MSR to the existing handlers.
pub(crate) fn identity_msr_write(msr: u32, value: u64) -> Option<Result<(), MsrError>> {
    if !identity_enabled() || !(0x4000_0000..=0x4000_01ff).contains(&msr) {
        return None;
    }
    let result = match (msr, value) {
        (MSR_TSC_INVARIANT_CONTROL, 0 | 1) => {
            TSC_INVARIANT_CONTROL.store(value, Ordering::Relaxed);
            Ok(())
        }
        _ => Err(MsrError::InvalidAccess),
    };
    tracing::info!(
        msr = format_args!("{msr:#x}"),
        value,
        ?result,
        "spike: identity MSR write"
    );
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_leaves_match_the_contract() {
        let leaves = virt::CpuidLeafSet::new(identity_cpuid_leaves(8));
        let r = |f| leaves.result(f, 0, &[0xdead_beef; 4]);
        assert_eq!(
            r(0x4000_0000),
            [0x4000_0005, 0x7263_694d, 0x666f_736f, 0x7648_2074]
        );
        assert_eq!(r(0x4000_0001), [0x3123_7648, 0, 0, 0]);
        assert_eq!(r(0x4000_0003), [0x8860, 0, 0, 0x100]);
        assert_eq!(r(0x4000_0004), [0, 0xffff_ffff, 0, 0]);
        assert_eq!(r(0x4000_0005), [8, 8, 0, 0]);
        for f in [0x4000_0006, 0x4000_0081, 0x4000_0082, 0x4000_00ff] {
            assert_eq!(r(f), [0; 4], "{f:#x}");
        }
        let leaf1 = r(1);
        assert_eq!(leaf1[2] & (1 << 31), 1 << 31);
        assert_eq!(leaf1[2] & (1 << 24), 0);
        assert_eq!(r(0x8000_0007)[3] & (1 << 8), 1 << 8);
        assert_eq!(leaves.result(7, 0, &[!0; 4])[1] & 2, 0);
        assert_eq!(leaves.result(7, 1, &[!0; 4])[1] & 2, 2);
    }
}
