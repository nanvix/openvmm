// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! LAPIC timer checks and advancement for an NVX time ABI snapshot, and the
//! LAPIC state that a host pause holds while the vCPUs are stopped.

use super::StateEvent;
use super::VpEvent;
use super::VpSet;
use anyhow::Context as _;
use futures::future::TryJoinAll;
use hvdef::Vtl;
use mesh::rpc::RpcSend;
use virt::Processor;
use virt::vp::AccessVpState;

/// A request to advance the LAPIC timer of a stopped VP by the restore
/// downtime.
#[derive(Debug, Copy, Clone)]
pub(super) struct LapicAdvance {
    downtime_ns: u64,
    apic_hz: u64,
}

/// Checks the LAPIC timer of the stopped VP `vp` (`E_LAPIC_PERIODIC`,
/// `E_LAPIC_TSC_DEADLINE`).
pub(super) fn check_one_shot_timer(vp: &mut impl Processor) -> anyhow::Result<()> {
    let apic = vp
        .access_state(Vtl::Vtl0)
        .apic()
        .context("failed to read stopped LAPIC")?;
    apic.check_one_shot_timer()?;
    Ok(())
}

impl LapicAdvance {
    /// Advances the one-shot LAPIC timer of the stopped VP `vp` and sets its
    /// LAPIC state again, even when no timer is armed: KVM derives its timer
    /// deadline from the guest TSC when the LAPIC state is set.
    pub(super) fn apply(self, vp: &mut impl Processor) -> anyhow::Result<()> {
        let mut access = vp.access_state(Vtl::Vtl0);
        let mut apic = access.apic().context("failed to read stopped LAPIC")?;
        apic.advance_one_shot_timer(self.downtime_ns, self.apic_hz)?;
        access
            .set_apic(&apic)
            .context("failed to set the restored LAPIC")?;
        access
            .commit()
            .context("failed to commit the restored LAPIC")?;
        Ok(())
    }
}

/// Reads the LAPIC state of the stopped VP `vp`.
pub(super) fn get_lapic(vp: &mut impl Processor) -> anyhow::Result<virt::x86::vp::Apic> {
    vp.access_state(Vtl::Vtl0)
        .apic()
        .context("failed to read stopped LAPIC")
}

/// Sets the LAPIC state of the stopped VP `vp` to `apic`. Like
/// [`LapicAdvance::apply`], this makes KVM derive its timer deadline from the
/// current guest TSC.
pub(super) fn set_lapic(vp: &mut impl Processor, apic: &virt::x86::vp::Apic) -> anyhow::Result<()> {
    let mut access = vp.access_state(Vtl::Vtl0);
    access
        .set_apic(apic)
        .context("failed to set stopped LAPIC")?;
    access.commit().context("failed to commit stopped LAPIC")?;
    Ok(())
}

impl VpSet {
    /// Checks the LAPIC timer of every stopped VP: capture step 1 of the time
    /// ABI. Restore needs no separate check, because the LAPIC advance
    /// rejects the same timers.
    pub async fn check_one_shot_timers(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before checking LAPIC timers"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(|rpc| VpEvent::State(StateEvent::CheckTimers(rpc)), ())
                    .await
                    .with_context(|| format!("vp{index} LAPIC timer"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }

    /// Advances the one-shot LAPIC timer of every stopped VP by `downtime_ns`
    /// at `apic_hz` and sets every VP's LAPIC state again: restore step 14 of
    /// the time ABI, after the synchronized TSC set. A periodic or
    /// TSC-deadline timer fails it (`E_LAPIC_PERIODIC`,
    /// `E_LAPIC_TSC_DEADLINE`) before any VP runs.
    pub async fn advance_lapic_timers(
        &mut self,
        downtime_ns: u64,
        apic_hz: u64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before advancing LAPIC timers"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(
                        |rpc| VpEvent::State(StateEvent::AdvanceLapic(rpc)),
                        LapicAdvance {
                            downtime_ns,
                            apic_hz,
                        },
                    )
                    .await
                    .with_context(|| format!("vp{index} LAPIC timer advance"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }

    /// Reads the LAPIC state of every stopped VP, in VP order.
    pub async fn get_lapics(&mut self) -> anyhow::Result<Vec<virt::x86::vp::Apic>> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before reading LAPIC state"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(|rpc| VpEvent::State(StateEvent::GetLapic(rpc)), ())
                    .await
                    .with_context(|| format!("vp{index} LAPIC read"))
            })
            .collect::<TryJoinAll<_>>()
            .await
    }

    /// Sets the LAPIC state of every stopped VP to `lapics`, in VP order.
    pub async fn set_lapics(&mut self, lapics: Vec<virt::x86::vp::Apic>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before setting LAPIC state"
        );
        anyhow::ensure!(
            lapics.len() == self.vps.len(),
            "{} LAPIC states were supplied for {} vCPUs",
            lapics.len(),
            self.vps.len()
        );
        self.vps
            .iter()
            .zip(lapics)
            .enumerate()
            .map(|(index, (vp, apic))| async move {
                vp.send
                    .call_failable(
                        |rpc| VpEvent::State(StateEvent::SetLapic(rpc)),
                        Box::new(apic),
                    )
                    .await
                    .with_context(|| format!("vp{index} LAPIC set"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::boundary::tests::vp_set;
    use super::*;
    use futures::FutureExt as _;
    use test_with_tracing::test;
    use virt::x86::vp::Apic;

    fn apic(index: u32) -> Apic {
        Apic {
            apic_base: 0xfee0_0900,
            registers: [index; 64],
            auto_eoi: [0; 8],
        }
    }

    #[test]
    fn lapic_state_is_read_and_set_in_vp_order() {
        let (mut vps, mut runners) = vp_set(2);

        let mut get = Box::pin(vps.get_lapics());
        assert!(get.as_mut().now_or_never().is_none());
        for (index, runner) in runners.iter_mut().enumerate().rev() {
            let Ok(VpEvent::State(StateEvent::GetLapic(rpc))) = runner.recv.try_recv() else {
                panic!("expected a LAPIC read");
            };
            rpc.complete(Ok(apic(index as u32)));
        }
        assert_eq!(get.now_or_never().unwrap().unwrap(), [apic(0), apic(1)]);

        let mut set = Box::pin(vps.set_lapics(vec![apic(0), apic(1)]));
        assert!(set.as_mut().now_or_never().is_none());
        for (index, runner) in runners.iter_mut().enumerate() {
            let Ok(VpEvent::State(StateEvent::SetLapic(rpc))) = runner.recv.try_recv() else {
                panic!("expected a LAPIC set");
            };
            let (state, response) = rpc.split();
            assert_eq!(*state, apic(index as u32));
            response.complete(Ok(()));
        }
        set.now_or_never().unwrap().unwrap();
    }

    #[test]
    fn lapic_state_requires_stopped_vps_and_one_state_per_vp() {
        let (mut vps, mut runners) = vp_set(2);
        assert!(
            vps.set_lapics(vec![apic(0)])
                .now_or_never()
                .unwrap()
                .is_err()
        );
        vps.start();
        assert!(vps.get_lapics().now_or_never().unwrap().is_err());
        assert!(
            vps.set_lapics(vec![apic(0), apic(1)])
                .now_or_never()
                .unwrap()
                .is_err()
        );
        for runner in &mut runners {
            assert!(matches!(runner.recv.try_recv(), Ok(VpEvent::Start)));
            assert!(runner.recv.try_recv().is_err());
        }
    }
}
