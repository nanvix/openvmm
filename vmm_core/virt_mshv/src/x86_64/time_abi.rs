// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SPIKE (NVX time ABI v1): the guest-visible time identity of an MSHV
//! microVM.
//!
//! The guest sees a minimal Hyper-V identity, "Microsoft Hv" with the "Hv#1"
//! interface, that advertises only VP_INDEX, the frequency MSRs, and the
//! invariant-TSC control. Linux then takes the TSC and LAPIC rates from the
//! frequency MSRs, marks the TSC reliable, and registers `tsc` at
//! `device_initcall`, without kernel patches or clock command-line tokens.
//!
//! The identity MSRs are served by the VMM through MSR-index intercepts
//! ([`MsrRouting::Intercept`]), so their semantics are fixed by the ABI rather
//! than by the hypervisor build. [`MsrRouting::Native`] keeps the hypervisor's
//! own VP_INDEX and frequency MSRs and intercepts only TSC_INVARIANT_CONTROL,
//! which no synthetic processor feature enables.
//!
//! This is prototype code: the mode is selected with the
//! `OPENVMM_MSHV_TIME_ABI` environment variable (`intercept`, the default;
//! `native`; or `off` for the legacy guest view).

use crate::Error;
use crate::ErrorInner;
use hvdef::HvPartitionSyntheticProcessorFeatures;
use hvdef::hypercall::HV_INTERCEPT_ACCESS_MASK_READ_WRITE;
use inspect::Inspect;
use mshv_ioctls::VmFd;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Hyper-V VP index MSR.
pub(crate) const MSR_VP_INDEX: u32 = 0x4000_0002;
/// Hyper-V TSC frequency MSR.
pub(crate) const MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
/// Hyper-V LAPIC timer frequency MSR.
pub(crate) const MSR_APIC_FREQUENCY: u32 = 0x4000_0023;
/// Hyper-V invariant-TSC control MSR.
pub(crate) const MSR_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;

/// The identity MSRs, in the order the VMM serves them.
pub(crate) const IDENTITY_MSRS: [u32; 4] = [
    MSR_VP_INDEX,
    MSR_TSC_FREQUENCY,
    MSR_APIC_FREQUENCY,
    MSR_TSC_INVARIANT_CONTROL,
];

/// First and last hypervisor CPUID leaves that the ABI defines.
pub(crate) const HV_LEAF_FIRST: u32 = 0x4000_0000;
pub(crate) const HV_LEAF_MAX: u32 = 0x4000_0005;
/// Last leaf of the `0x400000xx` range, whose unlisted leaves read as zero.
pub(crate) const HV_LEAF_RANGE_LAST: u32 = 0x4000_00ff;

/// `0x40000003` EAX: HYPERCALL_AVAILABLE (5) | VP_INDEX_AVAILABLE (6) |
/// ACCESS_FREQUENCY_MSRS (11) | ACCESS_TSC_INVARIANT (15).
pub(crate) const PRIVILEGES_EAX: u32 = 1 << 5 | 1 << 6 | 1 << 11 | 1 << 15;
/// `0x40000003` EDX: FREQUENCY_MSRS_AVAILABLE (8).
pub(crate) const FEATURES_EDX: u32 = 1 << 8;
/// `0x40000002` placeholder until the specification fixes the NVX version.
pub(crate) const NVX_BUILD: u32 = 1;
pub(crate) const NVX_VERSION: u32 = 0x0001_0000;

/// How the identity MSRs reach their handler.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub(crate) enum MsrRouting {
    /// Every identity MSR is intercepted and served by the VMM.
    Intercept,
    /// The hypervisor serves VP_INDEX and the frequency MSRs through the
    /// synthetic processor features; only TSC_INVARIANT_CONTROL is
    /// intercepted.
    Native,
}

/// CPUID coverage of the `0x400000xx` range.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub(crate) enum LeafCoverage {
    /// Override every leaf of the range, zeroing the unlisted ones.
    All,
    /// Override only the ABI leaves. Without synthetic processor features the
    /// hypervisor reports no `0x400000xx` leaves, so the others read as zero.
    Abi,
}

/// Time ABI configuration of a partition.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub(crate) struct TimeAbiMode {
    pub routing: MsrRouting,
    pub leaves: LeafCoverage,
    /// Sets CPUID.80000007H:EDX[8] even when the hypervisor does not expose
    /// an invariant TSC to child partitions (Azure nested MSHV).
    pub force_invariant_tsc: bool,
}

impl TimeAbiMode {
    /// Selects the time ABI for a partition. Only non-isolated microVM
    /// partitions without the Hyper-V guest interface use it.
    pub(crate) fn select(
        versioned_cpu_contract: bool,
        hv_configured: bool,
        isolated: bool,
    ) -> Option<Self> {
        if !versioned_cpu_contract || hv_configured || isolated {
            return None;
        }
        let routing = match std::env::var("OPENVMM_MSHV_TIME_ABI").as_deref() {
            Ok("off") => return None,
            Ok("native") => MsrRouting::Native,
            _ => MsrRouting::Intercept,
        };
        let leaves = match std::env::var("OPENVMM_MSHV_TIME_ABI_LEAVES").as_deref() {
            Ok("all") => LeafCoverage::All,
            _ if routing == MsrRouting::Native => LeafCoverage::All,
            _ => LeafCoverage::Abi,
        };
        let force_invariant_tsc =
            std::env::var("OPENVMM_MSHV_TIME_ABI_INVTSC").as_deref() == Ok("force");
        Some(Self {
            routing,
            leaves,
            force_invariant_tsc,
        })
    }

    /// Returns the synthetic processor features to set before the partition
    /// is initialized, if any.
    pub(crate) fn synthetic_features(&self) -> Option<HvPartitionSyntheticProcessorFeatures> {
        match self.routing {
            MsrRouting::Intercept => None,
            MsrRouting::Native => Some(
                HvPartitionSyntheticProcessorFeatures::new()
                    .with_hypervisor_present(true)
                    .with_hv1(true)
                    .with_access_vp_index(true)
                    .with_access_frequency_regs(true),
            ),
        }
    }

    /// Returns the MSRs to intercept.
    pub(crate) fn intercepted_msrs(&self) -> &'static [u32] {
        match self.routing {
            MsrRouting::Intercept => &IDENTITY_MSRS,
            MsrRouting::Native => &[MSR_TSC_INVARIANT_CONTROL],
        }
    }
}

/// Installs the MSR-index intercepts of `mode`.
pub(crate) fn install_msr_intercepts(vmfd: &VmFd, mode: &TimeAbiMode) -> Result<(), Error> {
    for &msr in mode.intercepted_msrs() {
        vmfd.install_intercept(msr_index_intercept(msr))
            .map_err(|e| ErrorInner::InstallIntercept(e.into()))?;
    }
    Ok(())
}

pub(crate) fn msr_index_intercept(msr: u32) -> mshv_bindings::mshv_install_intercept {
    mshv_bindings::mshv_install_intercept {
        access_type_mask: HV_INTERCEPT_ACCESS_MASK_READ_WRITE,
        intercept_type: mshv_bindings::hv_intercept_type_HV_INTERCEPT_TYPE_X64_MSR_INDEX,
        intercept_parameter: mshv_bindings::hv_intercept_parameters {
            as_uint64: u64::from(msr),
        },
    }
}

/// Returns the CPUID results of the time ABI: the hypervisor identity leaves
/// and the time-related feature bits.
pub(crate) fn cpuid_leaves(vp_capacity: u32, mode: &TimeAbiMode) -> Vec<virt::CpuidLeaf> {
    let signature = |s: &[u8; 4]| u32::from_le_bytes(*s);
    let last = match mode.leaves {
        LeafCoverage::All => HV_LEAF_RANGE_LAST,
        LeafCoverage::Abi => HV_LEAF_MAX,
    };
    let mut leaves: Vec<_> = (HV_LEAF_FIRST..=last)
        .map(|function| {
            let result = match function {
                0x4000_0000 => [
                    HV_LEAF_MAX,
                    signature(b"Micr"),
                    signature(b"osof"),
                    signature(b"t Hv"),
                ],
                0x4000_0001 => [signature(b"Hv#1"), 0, 0, 0],
                0x4000_0002 => [NVX_BUILD, NVX_VERSION, 0, 0],
                0x4000_0003 => [PRIVILEGES_EAX, 0, 0, FEATURES_EDX],
                0x4000_0004 => [0, !0, 0, 0],
                0x4000_0005 => [vp_capacity, vp_capacity, 0, 0],
                _ => [0; 4],
            };
            virt::CpuidLeaf::new(function, result)
        })
        .collect();

    // CPUID.1: the hypervisor bit is set and TSC-deadline is hidden.
    leaves.push(virt::CpuidLeaf::new(1, [0, 0, 1 << 31, 0]).masked([0, 0, 1 << 31 | 1 << 24, 0]));
    // CPUID.6: ARAT only; no APERF/MPERF, HWP, or other power management.
    leaves.push(virt::CpuidLeaf::new(6, [1 << 2, 0, 0, 0]));
    // CPUID.7.0: no TSC_ADJUST.
    leaves.push(
        virt::CpuidLeaf::new(7, [0; 4])
            .indexed(0)
            .masked([0, 1 << 1, 0, 0]),
    );
    // CPUID.0xA: no PMU.
    leaves.push(virt::CpuidLeaf::new(0xa, [0; 4]));
    if mode.force_invariant_tsc {
        // CPUID.80000007H: invariant TSC.
        leaves.push(virt::CpuidLeaf::new(0x8000_0007, [0, 0, 0, 1 << 8]).masked([0, 0, 0, 1 << 8]));
    }
    leaves
}

/// An intercepted access to an identity MSR.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MsrAccess {
    Read,
    Write(u64),
}

/// The outcome of an identity MSR access.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MsrOutcome {
    /// The read completes with this value.
    Value(u64),
    /// The write completes.
    Written,
    /// The access raises #GP.
    Fault,
}

/// Partition-wide time ABI state.
#[derive(Debug, Inspect)]
pub(crate) struct TimeAbiState {
    pub mode: TimeAbiMode,
    /// The declared guest TSC rate: the boot rate, or the snapshot's rate
    /// after a restore. Never the destination rate.
    #[inspect(with = "|x| x.load(Ordering::Relaxed)")]
    pub tsc_frequency_hz: AtomicU64,
    pub apic_frequency_hz: u64,
    /// Last value written to TSC_INVARIANT_CONTROL.
    ///
    /// SPIKE: not part of the saved state yet, so it reads 0 after a restore.
    #[inspect(with = "|x| x.load(Ordering::Relaxed)")]
    pub tsc_invariant_control: AtomicU64,
}

impl TimeAbiState {
    pub(crate) fn new(mode: TimeAbiMode, tsc_frequency_hz: u64, apic_frequency_hz: u64) -> Self {
        Self {
            mode,
            tsc_frequency_hz: AtomicU64::new(tsc_frequency_hz),
            apic_frequency_hz,
            tsc_invariant_control: AtomicU64::new(0),
        }
    }

    /// Serves an access to an identity MSR, per the ABI MSR table.
    pub(crate) fn access_msr(&self, vp_index: u32, msr: u32, access: MsrAccess) -> MsrOutcome {
        match (msr, access) {
            (MSR_VP_INDEX, MsrAccess::Read) => MsrOutcome::Value(vp_index.into()),
            (MSR_TSC_FREQUENCY, MsrAccess::Read) => {
                MsrOutcome::Value(self.tsc_frequency_hz.load(Ordering::Relaxed))
            }
            (MSR_APIC_FREQUENCY, MsrAccess::Read) => MsrOutcome::Value(self.apic_frequency_hz),
            (MSR_TSC_INVARIANT_CONTROL, MsrAccess::Read) => {
                MsrOutcome::Value(self.tsc_invariant_control.load(Ordering::Relaxed))
            }
            (MSR_TSC_INVARIANT_CONTROL, MsrAccess::Write(value @ (0 | 1))) => {
                self.tsc_invariant_control.store(value, Ordering::Relaxed);
                MsrOutcome::Written
            }
            _ => MsrOutcome::Fault,
        }
    }
}

/// Returns the pending-event register value that raises #GP(0).
pub(crate) fn gp_fault_event() -> u128 {
    hvdef::HvX64PendingExceptionEvent::new()
        .with_event_pending(true)
        .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
        .with_vector(x86defs::Exception::GENERAL_PROTECTION_FAULT.0.into())
        .with_deliver_error_code(true)
        .with_error_code(0)
        .into()
}

impl crate::MshvProcessor<'_> {
    /// Completes an intercepted identity MSR access, or raises #GP.
    pub(crate) fn handle_time_abi_msr_intercept(&mut self, message: &hvdef::HvMessage) {
        use crate::VcpuFdExt;
        let info = message.as_message::<hvdef::HvX64MsrInterceptMessage>();
        let access = if info.header.intercept_access_type == hvdef::HvInterceptAccessType::WRITE {
            MsrAccess::Write(info.rdx << 32 | (info.rax & 0xffff_ffff))
        } else {
            MsrAccess::Read
        };
        let outcome = match &self.partition.time_abi {
            Some(state) => state.access_msr(self.vpindex.index(), info.msr_number, access),
            None => MsrOutcome::Fault,
        };
        tracing::debug!(
            vp = self.vpindex.index(),
            msr = info.msr_number,
            ?access,
            ?outcome,
            "time ABI MSR access"
        );
        let next_rip = info.header.rip + u64::from(info.header.instruction_len());
        match outcome {
            MsrOutcome::Value(value) => {
                let rp = self.runner.reg_page();
                rp.gp_registers[x86emu::Gp::RAX as usize] = value & 0xffff_ffff;
                rp.gp_registers[x86emu::Gp::RDX as usize] = value >> 32;
                rp.rip = next_rip;
                rp.dirty.set_general_purpose(true);
                rp.dirty.set_instruction_pointer(true);
            }
            MsrOutcome::Written => {
                let rp = self.runner.reg_page();
                rp.rip = next_rip;
                rp.dirty.set_instruction_pointer(true);
            }
            MsrOutcome::Fault => {
                self.runner
                    .vcpufd
                    .set_hvdef_regs(&[hvdef::hypercall::HvRegisterAssoc::from((
                        hvdef::HvX64RegisterName::PendingEvent0,
                        gp_fault_event(),
                    ))])
                    .expect("failed to inject #GP for an identity MSR access");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(leaves: LeafCoverage, force_invariant_tsc: bool) -> TimeAbiMode {
        TimeAbiMode {
            routing: MsrRouting::Intercept,
            leaves,
            force_invariant_tsc,
        }
    }

    fn state() -> TimeAbiState {
        TimeAbiState::new(mode(LeafCoverage::Abi, false), 2_194_844_000, 200_000_000)
    }

    #[test]
    fn identity_msrs_follow_the_abi_table() {
        let state = state();
        assert_eq!(
            state.access_msr(3, MSR_VP_INDEX, MsrAccess::Read),
            MsrOutcome::Value(3)
        );
        assert_eq!(
            state.access_msr(0, MSR_TSC_FREQUENCY, MsrAccess::Read),
            MsrOutcome::Value(2_194_844_000)
        );
        assert_eq!(
            state.access_msr(0, MSR_APIC_FREQUENCY, MsrAccess::Read),
            MsrOutcome::Value(200_000_000)
        );
        for msr in [MSR_VP_INDEX, MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY] {
            assert_eq!(
                state.access_msr(0, msr, MsrAccess::Write(0)),
                MsrOutcome::Fault
            );
        }
    }

    #[test]
    fn tsc_invariant_control_accepts_only_zero_and_one() {
        let state = state();
        let read =
            |state: &TimeAbiState| state.access_msr(1, MSR_TSC_INVARIANT_CONTROL, MsrAccess::Read);
        assert_eq!(read(&state), MsrOutcome::Value(0));
        for value in [1, 0, 1] {
            assert_eq!(
                state.access_msr(1, MSR_TSC_INVARIANT_CONTROL, MsrAccess::Write(value)),
                MsrOutcome::Written
            );
            assert_eq!(read(&state), MsrOutcome::Value(value));
        }
        for value in [2, 3, 1 << 63 | 1, u64::MAX] {
            assert_eq!(
                state.access_msr(1, MSR_TSC_INVARIANT_CONTROL, MsrAccess::Write(value)),
                MsrOutcome::Fault
            );
            assert_eq!(read(&state), MsrOutcome::Value(1));
        }
    }

    #[test]
    fn other_msrs_fault() {
        let state = state();
        for msr in [
            0x4000_0000,
            0x4000_0001,
            0x4000_0020,
            0x4000_0021,
            0x4000_01ff,
        ] {
            assert_eq!(state.access_msr(0, msr, MsrAccess::Read), MsrOutcome::Fault);
            assert_eq!(
                state.access_msr(0, msr, MsrAccess::Write(0)),
                MsrOutcome::Fault
            );
        }
    }

    #[test]
    fn identity_leaves_match_the_abi() {
        let leaves = virt::CpuidLeafSet::new(cpuid_leaves(8, &mode(LeafCoverage::All, false)));
        let leaf = |function| leaves.result(function, 0, &[0xdead_beef; 4]);
        assert_eq!(
            leaf(0x4000_0000),
            [0x4000_0005, 0x7263_694d, 0x666f_736f, 0x7648_2074]
        );
        assert_eq!(leaf(0x4000_0001), [0x3123_7648, 0, 0, 0]);
        assert_eq!(leaf(0x4000_0003), [0x8860, 0, 0, 0x100]);
        assert_eq!(leaf(0x4000_0004), [0, 0xffff_ffff, 0, 0]);
        assert_eq!(leaf(0x4000_0005), [8, 8, 0, 0]);
        for function in 0x4000_0006..=0x4000_00ff {
            assert_eq!(leaf(function), [0; 4], "leaf {function:#x}");
        }
        let leaf1 = leaves.result(1, 0, &[0, 0, 1 << 24, 0]);
        assert_eq!(leaf1[2], 1 << 31);
        assert_eq!(leaves.result(6, 0, &[!0; 4]), [1 << 2, 0, 0, 0]);
        assert_eq!(leaves.result(7, 0, &[!0; 4])[1], !(1 << 1));
        assert_eq!(leaves.result(0xa, 0, &[!0; 4]), [0; 4]);
        assert_eq!(leaves.result(0x8000_0007, 0, &[0; 4]), [0; 4]);
    }

    #[test]
    fn forced_invariant_tsc_sets_only_its_bit() {
        let leaves = virt::CpuidLeafSet::new(cpuid_leaves(2, &mode(LeafCoverage::Abi, true)));
        assert_eq!(
            leaves.result(0x8000_0007, 0, &[1, 2, 3, 4]),
            [1, 2, 3, 4 | 1 << 8]
        );
    }

    #[test]
    fn abi_coverage_overrides_only_the_abi_leaves() {
        let leaves = cpuid_leaves(1, &mode(LeafCoverage::Abi, false));
        assert!(
            leaves
                .iter()
                .filter(|leaf| leaf.function >= HV_LEAF_FIRST)
                .all(|leaf| leaf.function <= HV_LEAF_MAX)
        );
    }
}

/// SPIKE: hardware probes of the MSR-serving mechanisms and the TSC freeze.
///
/// Run on a host with `/dev/mshv`:
/// `virt_mshv-<hash> --ignored --nocapture --test-threads=1 time_abi::hw_probe`
#[cfg(test)]
mod hw_probe {
    use super::*;
    use crate::VcpuFdExt;
    use crate::x86_64::partition_create_args;
    use crate::x86_64::tsc::with_features1;
    use hvdef::HvMessage;
    use hvdef::HvMessageType;
    use hvdef::HvPartitionPropertyCode;
    use hvdef::HvX64RegisterName;
    use hvdef::HvX64SegmentRegister;
    use hvdef::HvX64TableRegister;
    use hvdef::hypercall::HvRegisterAssoc;
    use mshv_ioctls::Mshv;
    use mshv_ioctls::VcpuFd;
    use mshv_ioctls::set_bits;
    use std::fmt::Write as _;
    use std::time::Instant;

    const MEM_SIZE: usize = 0x10_0000;
    const RDMSR_AT: u64 = 0x1000;
    const WRMSR_AT: u64 = 0x1010;
    const CPUID_AT: u64 = 0x1020;
    const RDTSC_AT: u64 = 0x1030;
    const PORT_DONE: u16 = 0x80;
    const PORT_GP: u16 = 0x81;
    const PORT_UD: u16 = 0x82;
    const PORT_OTHER: u16 = 0x83;
    /// Value returned by the probe's MSR intercept handler for reads.
    const INTERCEPT_READ_PATTERN: u64 = 0xc0de_0000_0000_0000;

    struct Memory(*mut u8);

    impl Memory {
        fn new() -> Self {
            // SAFETY: anonymous private mapping with no aliasing.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    MEM_SIZE,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            assert_ne!(ptr, libc::MAP_FAILED);
            Self(ptr.cast())
        }

        fn write(&self, gpa: usize, bytes: &[u8]) {
            assert!(gpa + bytes.len() <= MEM_SIZE);
            // SAFETY: the range is inside the mapping.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.0.add(gpa), bytes.len()) }
        }
    }

    impl Drop for Memory {
        fn drop(&mut self) {
            // SAFETY: the mapping was created by `new`.
            unsafe { libc::munmap(self.0.cast(), MEM_SIZE) };
        }
    }

    #[derive(Default)]
    struct Config {
        name: &'static str,
        synthetic: Option<HvPartitionSyntheticProcessorFeatures>,
        unimplemented_msr_action: Option<u64>,
        msr_index_intercepts: Vec<u32>,
        msr_intercept_all: bool,
        cpuid: Vec<virt::CpuidLeaf>,
        /// Raise #GP from the intercept handler for writes of this value.
        inject_gp_on_write: Option<u64>,
    }

    struct Vm {
        vmfd: VmFd,
        vcpu: VcpuFd,
        _memory: Memory,
        inject_gp_on_write: Option<u64>,
    }

    #[derive(Debug)]
    enum Exit {
        Done,
        Gp,
        Ud,
        OtherException,
        Unexpected,
    }

    fn unexpected(log: &mut String, reason: String) -> Exit {
        write!(log, " [unexpected: {reason}]").unwrap();
        Exit::Unexpected
    }

    fn abi_features() -> HvPartitionSyntheticProcessorFeatures {
        HvPartitionSyntheticProcessorFeatures::new()
            .with_hypervisor_present(true)
            .with_hv1(true)
            .with_access_vp_index(true)
            .with_access_frequency_regs(true)
    }

    fn register_cpuid(vmfd: &VmFd, leaf: &virt::CpuidLeaf) -> Result<(), mshv_ioctls::MshvError> {
        let input = hvdef::hypercall::RegisterInterceptResultCpuid {
            partition_id: 0,
            vp_index: hvdef::HV_ANY_VP,
            intercept_type: hvdef::hypercall::HvInterceptType::HvInterceptTypeX64Cpuid,
            parameters: hvdef::hypercall::HvRegisterX64CpuidResultParameters {
                input: hvdef::hypercall::HvRegisterX64CpuidResultParametersInput {
                    eax: leaf.function,
                    ecx: leaf.index.unwrap_or(0),
                    subleaf_specific: u8::from(leaf.index.is_some()),
                    always_override: 1,
                    padding: 0,
                },
                result: hvdef::hypercall::HvRegisterX64CpuidResultParametersOutput {
                    eax: leaf.result[0],
                    eax_mask: leaf.mask[0],
                    ebx: leaf.result[1],
                    ebx_mask: leaf.mask[1],
                    ecx: leaf.result[2],
                    ecx_mask: leaf.mask[2],
                    edx: leaf.result[3],
                    edx_mask: leaf.mask[3],
                },
            },
            _reserved: 0,
        };
        let mut args = mshv_bindings::mshv_root_hvcall {
            code: hvdef::HypercallCode::HvCallRegisterInterceptResult.0,
            in_sz: size_of_val(&input) as u16,
            in_ptr: std::ptr::addr_of!(input) as u64,
            ..Default::default()
        };
        vmfd.hvcall(&mut args)
    }

    fn real_mode_segment(code: bool) -> HvX64SegmentRegister {
        HvX64SegmentRegister {
            base: 0,
            limit: 0xffff,
            selector: 0,
            attributes: if code { 0x9b } else { 0x93 },
        }
    }

    impl Vm {
        fn new(config: &Config, log: &mut String) -> Self {
            let mshv = Mshv::new().unwrap();
            let args = with_features1(
                partition_create_args(&virt::ProtoPartitionIsolation::None, false, false).unwrap(),
                true,
            );
            let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
            if let Some(features) = config.synthetic {
                let result = vmfd.set_partition_property(
                    HvPartitionPropertyCode::SyntheticProcFeatures.0,
                    u64::from(features),
                );
                writeln!(
                    log,
                    "  set SyntheticProcFeatures={:#x}: {result:?}",
                    u64::from(features)
                )
                .unwrap();
            }
            vmfd.initialize().unwrap();
            if let Some(action) = config.unimplemented_msr_action {
                let result = vmfd.set_partition_property(
                    HvPartitionPropertyCode::UnimplementedMsrAction.0,
                    action,
                );
                writeln!(log, "  set UnimplementedMsrAction={action}: {result:?}").unwrap();
            }
            for &msr in &config.msr_index_intercepts {
                let result = vmfd.install_intercept(msr_index_intercept(msr));
                writeln!(log, "  install MSR_INDEX intercept {msr:#x}: {result:?}").unwrap();
            }
            if config.msr_intercept_all {
                let result = vmfd.install_intercept(mshv_bindings::mshv_install_intercept {
                    access_type_mask: HV_INTERCEPT_ACCESS_MASK_READ_WRITE,
                    intercept_type: mshv_bindings::hv_intercept_type_HV_INTERCEPT_TYPE_X64_MSR,
                    intercept_parameter: mshv_bindings::hv_intercept_parameters { as_uint64: 0 },
                });
                writeln!(log, "  install X64_MSR (all) intercept: {result:?}").unwrap();
            }
            if !config.cpuid.is_empty() {
                let started = Instant::now();
                let mut failures = 0;
                let mut first_error = None;
                for leaf in &config.cpuid {
                    if let Err(error) = register_cpuid(&vmfd, leaf) {
                        failures += 1;
                        first_error.get_or_insert(format!("{:#x}: {error:?}", leaf.function));
                    }
                }
                writeln!(
                    log,
                    "  registered {} CPUID results in {} us, {failures} failed {first_error:?}",
                    config.cpuid.len(),
                    started.elapsed().as_micros()
                )
                .unwrap();
            }

            let memory = Memory::new();
            // Real-mode IVT: #UD (6) and #GP (13) report through distinct
            // ports; every other vector reports through PORT_OTHER.
            for vector in 0..32u16 {
                let handler: u16 = match vector {
                    6 => 0x0810,
                    13 => 0x0800,
                    _ => 0x0820,
                };
                memory.write(
                    vector as usize * 4,
                    &[handler as u8, (handler >> 8) as u8, 0, 0],
                );
            }
            let out_and_spin = |port: u16| [0xe6, port as u8, 0xeb, 0xfe];
            memory.write(0x0800, &out_and_spin(PORT_GP));
            memory.write(0x0810, &out_and_spin(PORT_UD));
            memory.write(0x0820, &out_and_spin(PORT_OTHER));
            // rdmsr / wrmsr / cpuid / rdtsc, each followed by `out 0x80, al`.
            for (gpa, opcode) in [
                (RDMSR_AT, [0x0f, 0x32]),
                (WRMSR_AT, [0x0f, 0x30]),
                (CPUID_AT, [0x0f, 0xa2]),
                (RDTSC_AT, [0x0f, 0x31]),
            ] {
                memory.write(gpa as usize, &opcode);
                memory.write(gpa as usize + 2, &out_and_spin(PORT_DONE));
            }
            vmfd.map_user_memory(mshv_bindings::mshv_user_mem_region {
                size: MEM_SIZE as u64,
                guest_pfn: 0,
                userspace_addr: memory.0 as u64,
                flags: set_bits!(
                    u8,
                    mshv_bindings::MSHV_SET_MEM_BIT_WRITABLE,
                    mshv_bindings::MSHV_SET_MEM_BIT_EXECUTABLE
                ),
                rsvd: [0; 7],
            })
            .unwrap();
            let vcpu = vmfd.create_vcpu(0).unwrap();
            vcpu.set_hvdef_regs(&[
                HvRegisterAssoc::from((HvX64RegisterName::Cs, real_mode_segment(true))),
                HvRegisterAssoc::from((HvX64RegisterName::Ds, real_mode_segment(false))),
                HvRegisterAssoc::from((HvX64RegisterName::Es, real_mode_segment(false))),
                HvRegisterAssoc::from((HvX64RegisterName::Ss, real_mode_segment(false))),
                HvRegisterAssoc::from((HvX64RegisterName::Fs, real_mode_segment(false))),
                HvRegisterAssoc::from((HvX64RegisterName::Gs, real_mode_segment(false))),
                HvRegisterAssoc::from((
                    HvX64RegisterName::Idtr,
                    HvX64TableRegister {
                        pad: [0; 3],
                        limit: 0x3ff,
                        base: 0,
                    },
                )),
            ])
            .unwrap();
            Self {
                vmfd,
                vcpu,
                _memory: memory,
                inject_gp_on_write: config.inject_gp_on_write,
            }
        }

        fn run_at(&self, rip: u64, rax: u64, rcx: u64, rdx: u64, log: &mut String) -> Exit {
            self.vcpu
                .set_hvdef_regs(&[
                    HvRegisterAssoc::from((HvX64RegisterName::Rip, rip)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rax, rax)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rbx, 0u64)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rcx, rcx)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rdx, rdx)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rsp, 0x7000u64)),
                    HvRegisterAssoc::from((HvX64RegisterName::Rflags, 2u64)),
                ])
                .unwrap();
            for _ in 0..16 {
                let message: HvMessage = match crate::run_vp::run(&self.vcpu) {
                    Ok(message) => message,
                    Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
                    Err(error) => return unexpected(log, format!("run failed: {error}")),
                };
                match message.header.typ {
                    HvMessageType::HvMessageTypeX64IoPortIntercept => {
                        let info = message.as_message::<hvdef::HvX64IoPortInterceptMessage>();
                        return match info.port_number {
                            PORT_DONE => Exit::Done,
                            PORT_GP => Exit::Gp,
                            PORT_UD => Exit::Ud,
                            PORT_OTHER => Exit::OtherException,
                            port => unexpected(log, format!("port {port:#x}")),
                        };
                    }
                    HvMessageType::HvMessageTypeMsrIntercept => {
                        let info = message.as_message::<hvdef::HvX64MsrInterceptMessage>();
                        let write = info.header.intercept_access_type
                            == hvdef::HvInterceptAccessType::WRITE;
                        let value = info.rdx << 32 | (info.rax & 0xffff_ffff);
                        write!(
                            log,
                            " [intercept {} {:#x} value={value:#x} len={}]",
                            if write { "wr" } else { "rd" },
                            info.msr_number,
                            info.header.instruction_len()
                        )
                        .unwrap();
                        if write && self.inject_gp_on_write == Some(value) {
                            self.vcpu
                                .set_hvdef_regs(&[HvRegisterAssoc::from((
                                    HvX64RegisterName::PendingEvent0,
                                    gp_fault_event(),
                                ))])
                                .unwrap();
                            continue;
                        }
                        let next_rip = info.header.rip + u64::from(info.header.instruction_len());
                        let mut regs =
                            vec![HvRegisterAssoc::from((HvX64RegisterName::Rip, next_rip))];
                        if !write {
                            let value = INTERCEPT_READ_PATTERN | u64::from(info.msr_number);
                            regs.push(HvRegisterAssoc::from((
                                HvX64RegisterName::Rax,
                                value & 0xffff_ffff,
                            )));
                            regs.push(HvRegisterAssoc::from((HvX64RegisterName::Rdx, value >> 32)));
                        }
                        self.vcpu.set_hvdef_regs(&regs).unwrap();
                    }
                    typ => return unexpected(log, format!("message {typ:?}")),
                }
            }
            unexpected(log, "too many exits".into())
        }

        fn regs(&self) -> [u64; 4] {
            let mut regs = [
                HvRegisterAssoc::from((HvX64RegisterName::Rax, 0u64)),
                HvRegisterAssoc::from((HvX64RegisterName::Rbx, 0u64)),
                HvRegisterAssoc::from((HvX64RegisterName::Rcx, 0u64)),
                HvRegisterAssoc::from((HvX64RegisterName::Rdx, 0u64)),
            ];
            self.vcpu.get_hvdef_regs(&mut regs).unwrap();
            regs.map(|reg| reg.value.as_u64())
        }

        fn rdmsr(&self, msr: u32, log: &mut String) {
            write!(log, "    rdmsr {msr:#010x}:").unwrap();
            let exit = self.run_at(RDMSR_AT, 0, msr.into(), 0, log);
            match exit {
                Exit::Done => {
                    let [rax, _, _, rdx] = self.regs();
                    writeln!(
                        log,
                        " ok {:#x}",
                        (rdx & 0xffff_ffff) << 32 | (rax & 0xffff_ffff)
                    )
                    .unwrap();
                }
                exit => writeln!(log, " {exit:?}").unwrap(),
            }
        }

        fn wrmsr(&self, msr: u32, value: u64, log: &mut String) {
            write!(log, "    wrmsr {msr:#010x} <- {value:#x}:").unwrap();
            let exit = self.run_at(WRMSR_AT, value & 0xffff_ffff, msr.into(), value >> 32, log);
            writeln!(log, " {exit:?}").unwrap();
        }

        fn cpuid(&self, function: u32, log: &mut String) -> Option<[u32; 4]> {
            match self.run_at(CPUID_AT, function.into(), 0, 0, log) {
                Exit::Done => Some(self.regs().map(|r| r as u32)),
                exit => {
                    writeln!(log, "    cpuid {function:#x}: {exit:?}").unwrap();
                    None
                }
            }
        }
    }

    fn msr_matrix(vm: &Vm, log: &mut String) {
        for msr in [
            0x4000_0000,
            0x4000_0001,
            MSR_VP_INDEX,
            0x4000_0020,
            0x4000_0021,
            MSR_TSC_FREQUENCY,
            MSR_APIC_FREQUENCY,
            MSR_TSC_INVARIANT_CONTROL,
            0x4000_0103,
            0x4000_01ff,
            0x10,
            0x3b,
            0xe7,
            0xe8,
            0x6e0,
            0x1ad,
            0x10a,
        ] {
            vm.rdmsr(msr, log);
        }
        vm.wrmsr(0x4000_0000, 0x1, log);
        vm.wrmsr(MSR_VP_INDEX, 0, log);
        vm.wrmsr(MSR_TSC_FREQUENCY, 1, log);
        vm.wrmsr(MSR_APIC_FREQUENCY, 1, log);
        for value in [1, 0, 2, 1 << 63 | 1, 1] {
            vm.wrmsr(MSR_TSC_INVARIANT_CONTROL, value, log);
            vm.rdmsr(MSR_TSC_INVARIANT_CONTROL, log);
        }
        vm.wrmsr(0x4000_01ff, 0, log);
        vm.wrmsr(0x199, 0, log);
    }

    fn cpuid_dump(vm: &Vm, log: &mut String) {
        let mut zero = 0;
        for function in (0x4000_0000..=0x4000_00ff).chain([
            0x4000_0100,
            0x4000_0101,
            0x4000_0200,
            0x4000_ff00,
            1,
            6,
            0xa,
            0x15,
            0x16,
            0x8000_0007,
        ]) {
            let Some(result) = vm.cpuid(function, log) else {
                continue;
            };
            let native = vm.vcpu.get_cpuid_values(function, 0, 0, 0).ok();
            if result == [0; 4] && native.is_none_or(|n| n == [0; 4]) {
                zero += 1;
                continue;
            }
            writeln!(
                log,
                "    cpuid {function:#010x}: guest {:08x?} get_cpuid_values {:08x?}",
                result, native
            )
            .unwrap();
        }
        writeln!(log, "    cpuid: {zero} probed leaves were all zero").unwrap();
    }

    fn probe(config: Config, with_cpuid: bool) -> String {
        let mut log = String::new();
        writeln!(log, "== config {}", config.name).unwrap();
        let vm = Vm::new(&config, &mut log);
        for code in [
            HvPartitionPropertyCode::ProcessorClockFrequency,
            HvPartitionPropertyCode::ApicFrequency,
        ] {
            writeln!(
                log,
                "  property {code:?}: {:?}",
                vm.vmfd.get_partition_property(code.0)
            )
            .unwrap();
        }
        msr_matrix(&vm, &mut log);
        if with_cpuid {
            cpuid_dump(&vm, &mut log);
        }
        log
    }

    #[test]
    #[ignore = "requires /dev/mshv"]
    fn msr_mechanisms() {
        let abi_cpuid = cpuid_leaves(
            8,
            &TimeAbiMode {
                routing: MsrRouting::Intercept,
                leaves: LeafCoverage::All,
                force_invariant_tsc: true,
            },
        );
        let configs = [
            (
                Config {
                    name: "none (legacy microVM)",
                    ..Default::default()
                },
                true,
            ),
            (
                Config {
                    name: "synthetic abi features",
                    synthetic: Some(abi_features()),
                    ..Default::default()
                },
                true,
            ),
            (
                Config {
                    name: "synthetic abi + access_hypercall_regs",
                    synthetic: Some(abi_features().with_access_hypercall_regs(true)),
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "synthetic abi + MSR_INDEX 0x40000118",
                    synthetic: Some(abi_features()),
                    msr_index_intercepts: vec![MSR_TSC_INVARIANT_CONTROL],
                    inject_gp_on_write: Some(2),
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "synthetic abi + MSR_INDEX all identity MSRs",
                    synthetic: Some(abi_features()),
                    msr_index_intercepts: IDENTITY_MSRS.to_vec(),
                    inject_gp_on_write: Some(2),
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "no synthetic + MSR_INDEX all identity MSRs",
                    msr_index_intercepts: IDENTITY_MSRS.to_vec(),
                    inject_gp_on_write: Some(2),
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "no synthetic + MSR_INDEX all identity MSRs + abi CPUID",
                    msr_index_intercepts: IDENTITY_MSRS.to_vec(),
                    inject_gp_on_write: Some(2),
                    cpuid: abi_cpuid.clone(),
                    ..Default::default()
                },
                true,
            ),
            (
                Config {
                    name: "no synthetic + MSR_INDEX 0x10a (ARCH_CAPABILITIES)",
                    msr_index_intercepts: vec![0x10a],
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "no synthetic + X64_MSR (all)",
                    msr_intercept_all: true,
                    ..Default::default()
                },
                false,
            ),
            (
                Config {
                    name: "no synthetic + UnimplementedMsrAction=IGNORE_WRITE_READ_ZERO",
                    unimplemented_msr_action: Some(u64::from(
                        mshv_bindings::hv_unimplemented_msr_action_HV_UNIMPLEMENTED_MSR_ACTION_IGNORE_WRITE_READ_ZERO,
                    )),
                    ..Default::default()
                },
                false,
            ),
        ];
        for (config, with_cpuid) in configs {
            println!("{}", probe(config, with_cpuid));
        }
    }

    /// Freezes partition time, writes one TSC value to every VP, reads it
    /// back, thaws, and then bounds each VP's live TSC offset against the
    /// host TSC from the root.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn tsc_freeze_write_readback() {
        // xtask-fmt allow-target-arch cpu-intrinsic
        fn host_tsc() -> u64 {
            // SAFETY: RDTSC has no memory side effects.
            unsafe {
                core::arch::x86_64::_mm_lfence();
                core::arch::x86_64::_rdtsc()
            }
        }
        let tsc_reg = || [HvRegisterAssoc::from((HvX64RegisterName::Tsc, 0u64))];
        for vp_count in [1u8, 2, 4, 8] {
            let mshv = Mshv::new().unwrap();
            let args = with_features1(
                partition_create_args(&virt::ProtoPartitionIsolation::None, false, false).unwrap(),
                true,
            );
            let vmfd = crate::create_vm_with_retry(&mshv, &args).unwrap();
            vmfd.initialize().unwrap();
            let vps: Vec<_> = (0..vp_count)
                .map(|i| vmfd.create_vcpu(i).unwrap())
                .collect();
            // Let the VPs diverge from the partition-creation values first.
            std::thread::sleep(std::time::Duration::from_millis(5));
            let started = Instant::now();
            vmfd.set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 1)
                .unwrap();
            let frozen = started.elapsed();
            let mut bsp = tsc_reg();
            vps[0].get_hvdef_regs(&mut bsp).unwrap();
            let target = bsp[0].value.as_u64() + 1_000_000_000;
            let write_started = Instant::now();
            for vp in &vps {
                vp.set_hvdef_regs(&[HvRegisterAssoc::from((HvX64RegisterName::Tsc, target))])
                    .unwrap();
            }
            let written = write_started.elapsed();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let readback: Vec<u64> = vps
                .iter()
                .map(|vp| {
                    let mut reg = tsc_reg();
                    vp.get_hvdef_regs(&mut reg).unwrap();
                    reg[0].value.as_u64()
                })
                .collect();
            let thaw_started = Instant::now();
            vmfd.set_partition_property(HvPartitionPropertyCode::TimeFreeze.0, 0)
                .unwrap();
            let thawed = thaw_started.elapsed();
            // Bound guest-minus-host TSC per VP over several rounds.
            let mut bounds = vec![(i128::MIN, i128::MAX); vps.len()];
            for _ in 0..200 {
                for (vp, bound) in vps.iter().zip(bounds.iter_mut()) {
                    let mut reg = tsc_reg();
                    let before = host_tsc();
                    vp.get_hvdef_regs(&mut reg).unwrap();
                    let after = host_tsc();
                    let guest = i128::from(reg[0].value.as_u64());
                    bound.0 = bound.0.max(guest - i128::from(after));
                    bound.1 = bound.1.min(guest - i128::from(before));
                }
            }
            let lo = bounds.iter().map(|b| b.0).max().unwrap();
            let hi = bounds.iter().map(|b| b.1).min().unwrap();
            println!(
                "vp_count={vp_count} freeze={frozen:?} write_all={written:?} thaw={thawed:?} \
                 target={target:#x} readback_equal={} readback={readback:x?}",
                readback.iter().all(|&tsc| tsc == target)
            );
            for (index, (low, high)) in bounds.iter().enumerate() {
                println!(
                    "  vp{index} live offset (guest-host) in [{low}, {high}] cycles, width {}",
                    high - low
                );
            }
            println!(
                "  common offset interval [{lo}, {hi}] cycles: {}",
                if lo <= hi {
                    "consistent with one shared offset"
                } else {
                    "NO shared offset"
                }
            );
        }
    }
}
