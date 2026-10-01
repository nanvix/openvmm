// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SPIKE (time ABI v1, KVM): the guest-visible time identity of the microVM.
//!
//! The guest sees a minimal Hyper-V frequency and invariant-TSC identity
//! instead of KVM's signature and kvmclock:
//!
//! - CPUID `0x40000000..=0x40000005` carry the Hyper-V vendor, interface,
//!   version, feature, hint, and limit leaves. The other leaves the guest may
//!   probe read as zero.
//! - The identity MSRs (`0x40000000..=0x400001ff`) are served in userspace
//!   through a KVM MSR filter (option b), or by KVM's in-kernel Hyper-V
//!   emulation with `KVM_CAP_HYPERV_ENFORCE_CPUID` (option a, evaluation only).
//! - TSC-deadline, TSC_ADJUST, APERF/MPERF, and the PMU are hidden.
//! - On restore, every vCPU gets one common `KVM_VCPU_TSC_OFFSET`, computed
//!   for a single host instant.
//!
//! This is prototype code that answers the design questions of the spike. It
//! is not meant to be integrated as is.

use crate::KvmError;
use crate::KvmPartitionInner;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use virt::CpuidLeaf;
use virt::VpIndex;
use x86defs::cpuid::CpuidFunction;

/// How the identity MSRs reach their implementation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MsrRouting {
    /// Option b: `KVM_X86_SET_MSR_FILTER` denies the whole range, and
    /// `KVM_MSR_EXIT_REASON_FILTER` exits deliver every access to OpenVMM.
    Filter,
    /// Option a: KVM's in-kernel Hyper-V emulation, restricted to the CPUID
    /// privileges with `KVM_CAP_HYPERV_ENFORCE_CPUID`.
    Native,
}

impl MsrRouting {
    /// Selects the routing. The spike defaults to the filter; the
    /// `NVX_SPIKE_KVM_HV_MSRS=native` environment variable selects option a
    /// for its evaluation.
    pub(crate) fn from_env() -> Self {
        match std::env::var("NVX_SPIKE_KVM_HV_MSRS").as_deref() {
            Ok("native") => Self::Native,
            _ => Self::Filter,
        }
    }
}

pub(crate) const HV_MSR_FIRST: u32 = 0x4000_0000;
pub(crate) const HV_MSR_COUNT: u32 = 0x200;
const HV_X64_MSR_VP_INDEX: u32 = 0x4000_0002;
const HV_X64_MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
const HV_X64_MSR_APIC_FREQUENCY: u32 = 0x4000_0023;
pub(crate) const HV_X64_MSR_TSC_INVARIANT_CONTROL: u32 = 0x4000_0118;

/// Max hypervisor leaf. Linux requires at least `0x40000005`.
const MAX_LEAF: u32 = 0x4000_0005;
/// `0x40000003` EAX: HYPERCALL_AVAILABLE (5), VP_INDEX_AVAILABLE (6),
/// ACCESS_FREQUENCY_MSRS (11), and ACCESS_TSC_INVARIANT (15).
const FEATURES_EAX: u32 = (1 << 5) | (1 << 6) | (1 << 11) | (1 << 15);
/// `0x40000003` EDX: FREQUENCY_MSRS_AVAILABLE (8).
const FEATURES_EDX: u32 = 1 << 8;
/// SPIKE placeholder for the NVX build/version leaf (`0x40000002`); the spec
/// fixes the real values.
const VERSION_EAX: u32 = 1;
const VERSION_EBX: u32 = 1 << 16;

/// Returns whether `msr` is in the Hyper-V synthetic MSR range owned by the
/// time ABI.
pub(crate) fn is_identity_msr(msr: u32) -> bool {
    (HV_MSR_FIRST..HV_MSR_FIRST + HV_MSR_COUNT).contains(&msr)
}

/// Returns the identity CPUID leaves.
///
/// KVM returns the highest basic leaf for an Intel guest's CPUID query beyond
/// the max hypervisor leaf, instead of zeros. Leaves that a guest may probe
/// are therefore listed explicitly: `0x40000006..=0x4000000f` and
/// `0x40000080..=0x40000082` (Linux reads `0x40000081`/`0x40000082` in
/// `ms_hyperv_msi_ext_dest_id()`). Covering the whole `0x400000xx` range would
/// exceed KVM's 256-entry CPUID limit.
pub(crate) fn identity_leaves(vp_count: u32) -> Vec<CpuidLeaf> {
    let mut leaves = vec![
        CpuidLeaf::new(
            0x4000_0000,
            [
                MAX_LEAF,
                u32::from_le_bytes(*b"Micr"),
                u32::from_le_bytes(*b"osof"),
                u32::from_le_bytes(*b"t Hv"),
            ],
        ),
        CpuidLeaf::new(0x4000_0001, [u32::from_le_bytes(*b"Hv#1"), 0, 0, 0]),
        CpuidLeaf::new(0x4000_0002, [VERSION_EAX, VERSION_EBX, 0, 0]),
        CpuidLeaf::new(0x4000_0003, [FEATURES_EAX, 0, 0, FEATURES_EDX]),
        CpuidLeaf::new(0x4000_0004, [0, 0xffff_ffff, 0, 0]),
        CpuidLeaf::new(0x4000_0005, [vp_count, vp_count, 0, 0]),
    ];
    leaves.extend(
        (0x4000_0006..=0x4000_000f)
            .chain(0x4000_0080..=0x4000_0082)
            .map(|function| CpuidLeaf::new(function, [0; 4])),
    );
    leaves
}

/// Returns the masks that fix the time-related CPUID bits.
///
/// Hidden: TSC-deadline (1.ECX[24]), TSC_ADJUST (7.0.EBX[1]), APERF/MPERF
/// (6.ECX[0]), and the architectural PMU (leaf 0xA). Set: the hypervisor bit
/// (1.ECX[31]). Invariant TSC (0x80000007.EDX[8]) and ARAT (6.EAX[2]) pass
/// through from KVM, which reports them when the host has them.
pub(crate) fn time_bit_leaves() -> Vec<CpuidLeaf> {
    vec![
        CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, 1 << 31, 0]).masked([
            0,
            0,
            (1 << 31) | (1 << 24),
            0,
        ]),
        CpuidLeaf::new(CpuidFunction::ExtendedFeatures.0, [0; 4])
            .indexed(0)
            .masked([0, 1 << 1, 0, 0]),
        CpuidLeaf::new(CpuidFunction::PowerManagement.0, [0; 4]).masked([0, 0, 1 << 0, 0]),
        CpuidLeaf::new(CpuidFunction::PerformanceMonitoring.0, [0; 4]),
    ]
}

/// Returns the MSR filter that sends every identity MSR access to userspace.
pub(crate) fn msr_filter() -> kvm::MsrFilterRange {
    kvm::MsrFilterRange {
        base: HV_MSR_FIRST,
        nmsrs: HV_MSR_COUNT,
        read: true,
        write: true,
        // All bits clear: deny, so the access exits with
        // KVM_MSR_EXIT_REASON_FILTER.
        bitmap: vec![0; (HV_MSR_COUNT / 8) as usize],
    }
}

/// Partition-wide state of the userspace identity MSRs.
#[derive(Debug)]
pub(crate) struct TimeAbi {
    pub(crate) routing: MsrRouting,
    /// The guest's declared TSC rate (`0x40000022`): the boot rate, or the
    /// snapshot's rate after a restore. Never the destination rate.
    declared_tsc_hz: AtomicU64,
    /// Last value written to `HV_X64_MSR_TSC_INVARIANT_CONTROL`.
    ///
    /// SPIKE: not saved in the snapshot. The real time platform component
    /// must save and restore it.
    invariant_tsc_control: AtomicU64,
    /// Number of identity MSR exits served, to show that they only happen at
    /// boot.
    exits: AtomicU64,
}

/// The KVM in-kernel LAPIC uses a 1 GHz bus clock (no
/// `KVM_CAP_X86_APIC_BUS_CYCLES_NS` change).
const APIC_FREQUENCY_HZ: u64 = super::tsc::APIC_FREQUENCY_HZ;

impl TimeAbi {
    pub(crate) fn new(routing: MsrRouting, tsc_hz: u64) -> Self {
        Self {
            routing,
            declared_tsc_hz: tsc_hz.into(),
            invariant_tsc_control: 0.into(),
            exits: 0.into(),
        }
    }

    pub(crate) fn declared_tsc_hz(&self) -> u64 {
        self.declared_tsc_hz.load(Ordering::Relaxed)
    }

    pub(crate) fn set_declared_tsc_hz(&self, hz: u64) {
        self.declared_tsc_hz.store(hz, Ordering::Relaxed);
    }

    /// Serves a guest RDMSR of an identity MSR. `None` raises #GP.
    pub(crate) fn read_msr(&self, vp: VpIndex, msr: u32) -> Option<u64> {
        let value = match msr {
            HV_X64_MSR_VP_INDEX => Some(vp.index().into()),
            HV_X64_MSR_TSC_FREQUENCY => Some(self.declared_tsc_hz()),
            HV_X64_MSR_APIC_FREQUENCY => Some(APIC_FREQUENCY_HZ),
            HV_X64_MSR_TSC_INVARIANT_CONTROL => {
                Some(self.invariant_tsc_control.load(Ordering::Relaxed))
            }
            _ => None,
        };
        let exits = self.exits.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            vp = vp.index(),
            msr = format_args!("{msr:#x}"),
            ?value,
            exits,
            "time abi: identity MSR read"
        );
        value
    }

    /// Serves a guest WRMSR of an identity MSR. Returns false to raise #GP.
    pub(crate) fn write_msr(&self, vp: VpIndex, msr: u32, data: u64) -> bool {
        let ok = match msr {
            HV_X64_MSR_TSC_INVARIANT_CONTROL if data <= 1 => {
                self.invariant_tsc_control.store(data, Ordering::Relaxed);
                true
            }
            _ => false,
        };
        let exits = self.exits.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            vp = vp.index(),
            msr = format_args!("{msr:#x}"),
            data = format_args!("{data:#x}"),
            ok,
            exits,
            "time abi: identity MSR write"
        );
        ok
    }
}

/// The result of [`KvmPartitionInner::synchronize_restored_tsc`].
#[derive(Debug)]
pub(crate) struct TscSync {
    /// Downtime measured at the host instant of the offset computation.
    pub(crate) downtime: Duration,
}

impl KvmPartitionInner {
    /// Sets one common `KVM_VCPU_TSC_OFFSET` on every vCPU so that each guest
    /// TSC equals `capture_tsc + D * frequency_hz / 1e9` at a single host
    /// instant, where `D` is the host time elapsed since `capture_time`.
    ///
    /// `capture_tsc` defaults to the BSP's saved TSC, as recorded while its
    /// state was restored. Must run before any vCPU runs, and before the LAPIC
    /// timers are re-armed: KVM computes a TSC-based hardware timer deadline
    /// from the guest TSC at `KVM_SET_LAPIC` time.
    pub(crate) fn synchronize_restored_tsc(
        &self,
        capture_tsc: Option<u64>,
        frequency_hz: u64,
        capture_time: SystemTime,
    ) -> Result<TscSync, KvmError> {
        let capture_tsc = match capture_tsc {
            Some(tsc) => tsc,
            None => *self.bsp().restored_tsc.lock(),
        };
        let vps: Vec<_> = self
            .vps
            .iter()
            .map(|vp| self.kvm.vp(vp.vp_info().apic_id))
            .collect();
        if !vps[0].has_tsc_offset_attr() {
            return Err(KvmError::InvalidState(
                "host lacks KVM_VCPU_TSC_OFFSET (Linux 5.16+)",
            ));
        }

        // Offsets left by the IA32_TSC writes of the state restore, which go
        // through KVM's TSC synchronization heuristics. Logged as evidence.
        let before = vps
            .iter()
            .map(|vp| vp.tsc_offset())
            .collect::<Result<Vec<_>, _>>()?;

        // Pair a host clock sample with a host TSC sample.
        let h0 = safe_intrinsics::rdtsc();
        let now = SystemTime::now();
        let h1 = safe_intrinsics::rdtsc();
        let host_tsc = h0 + (h1 - h0) / 2;
        let downtime = now
            .duration_since(capture_time)
            .map_err(|_| KvmError::InvalidState("host clock is before the capture time"))?;
        let cycles = u64::try_from(downtime.as_nanos() * u128::from(frequency_hz) / 1_000_000_000)
            .map_err(|_| KvmError::SnapshotClockOverflow)?;
        let target = capture_tsc
            .checked_add(cycles)
            .ok_or(KvmError::SnapshotClockOverflow)?;
        // Guest TSC = host TSC + offset: the host-native rate is never scaled.
        let offset = target.wrapping_sub(host_tsc);

        let set_start = std::time::Instant::now();
        for vp in &vps {
            vp.set_tsc_offset(offset)?;
        }
        let set_elapsed = set_start.elapsed();

        // Read back: every vCPU must carry exactly the common offset, and its
        // TSC must equal the host TSC plus that offset (no scaling).
        let mut max_bracket = 0;
        for (index, vp) in vps.iter().enumerate() {
            let after = vp.tsc_offset()?;
            if after != offset {
                tracing::error!(
                    index,
                    after,
                    offset,
                    "time abi: TSC offset read-back mismatch"
                );
                return Err(KvmError::InvalidState("TSC offset read-back mismatch"));
            }
            let mut guest = [0];
            let a = safe_intrinsics::rdtsc();
            vp.get_msrs(&[x86defs::X86X_MSR_TSC], &mut guest)?;
            let b = safe_intrinsics::rdtsc();
            let guest_host = guest[0].wrapping_sub(offset);
            if guest_host < a || guest_host > b {
                tracing::error!(
                    index,
                    guest = guest[0],
                    a,
                    b,
                    "time abi: guest TSC is not host TSC plus the offset"
                );
                return Err(KvmError::InvalidState("guest TSC is scaled or offset"));
            }
            max_bracket = max_bracket.max(b - a);
        }

        tracing::info!(
            vps = vps.len(),
            capture_tsc,
            frequency_hz,
            downtime_ns = downtime.as_nanos() as u64,
            cycles,
            target,
            offset = format_args!("{offset:#x}"),
            before = ?before.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>(),
            before_equal = before.iter().all(|o| *o == before[0]),
            sample_window_cycles = h1 - h0,
            set_us = set_elapsed.as_micros() as u64,
            max_readback_window_cycles = max_bracket,
            "time abi: synchronized restored TSC with one common offset"
        );
        Ok(TscSync { downtime })
    }
}
