// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor partition operations used by the microVM profile.

use super::ArchPartition;
use super::BasicPartitionStateAccess;
use virt::Partition;
use virt::PartitionAccessState;
use virt::PartitionMemoryMapper;

/// Backend partition operations required by the microVM lifecycle.
pub trait MicrovmPartition: Send + Sync {
    /// Returns the effective x86 CPU compatibility contract.
    #[cfg(guest_arch = "x86_64")]
    fn cpu_compatibility_contract(&self) -> virt::x86::CpuCompatibilityContract;

    /// Returns the effective guest TSC frequency.
    fn tsc_frequency_hz(&self) -> anyhow::Result<Option<u64>>;

    /// Requests the effective guest TSC frequency.
    fn set_tsc_frequency_hz(&self, frequency_hz: u64) -> anyhow::Result<()>;

    /// Advances backend-specific guest clock state after snapshot downtime.
    fn advance_snapshot_time(&self, duration: std::time::Duration) -> anyhow::Result<()>;

    /// SPIKE (time ABI v1): synchronizes every VP's restored TSC at one host
    /// instant. Returns the downtime used, or `None` when the backend leaves
    /// the TSC to the per-VP advance.
    #[cfg(guest_arch = "x86_64")]
    fn synchronize_restored_tsc(
        &self,
        frequency_hz: u64,
        capture_time: std::time::SystemTime,
    ) -> anyhow::Result<Option<std::time::Duration>>;

    /// Returns the LAPIC interrupt clock frequency when available.
    fn apic_frequency_hz(&self) -> anyhow::Result<Option<u64>>;
}

impl<T> MicrovmPartition for T
where
    T: BasicPartitionStateAccess + ArchPartition + PartitionMemoryMapper + PartitionAccessState,
{
    #[cfg(guest_arch = "x86_64")]
    fn cpu_compatibility_contract(&self) -> virt::x86::CpuCompatibilityContract {
        Partition::cpu_compatibility_contract(self)
    }

    fn tsc_frequency_hz(&self) -> anyhow::Result<Option<u64>> {
        Ok(Partition::tsc_frequency_hz(self)?)
    }

    fn set_tsc_frequency_hz(&self, frequency_hz: u64) -> anyhow::Result<()> {
        Partition::set_tsc_frequency_hz(self, frequency_hz)?;
        Ok(())
    }

    fn advance_snapshot_time(&self, duration: std::time::Duration) -> anyhow::Result<()> {
        Partition::advance_snapshot_time(self, duration)?;
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn synchronize_restored_tsc(
        &self,
        frequency_hz: u64,
        capture_time: std::time::SystemTime,
    ) -> anyhow::Result<Option<std::time::Duration>> {
        Ok(Partition::synchronize_restored_tsc(
            self,
            frequency_hz,
            capture_time,
        )?)
    }

    fn apic_frequency_hz(&self) -> anyhow::Result<Option<u64>> {
        Ok(Partition::apic_frequency_hz(self)?)
    }
}
