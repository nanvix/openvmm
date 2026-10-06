// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host pause of a microVM: stop the vCPUs and devices, and hold guest time so
//! the guest observes no elapsed time across the pause.
//!
//! A pause takes VP 0's TSC and every VP's LAPIC state once the vCPUs have
//! stopped. Resume sets every VP's TSC back to the held value with the
//! backend's synchronized TSC set, and sets the held LAPIC state again, before
//! any vCPU runs. VM time stops and starts with the state units. Host UTC,
//! which the CMOS RTC follows, keeps running, so the guest's wall-clock
//! discipline steps `CLOCK_REALTIME` at its next poll.
//!
//! A guest reset processed while the host holds the pause reinitializes guest
//! time, so it discards the held time but keeps the pause: the reset VM starts
//! when the host resumes it. A failed reset ends the pause and leaves the VM
//! stopped.

use super::LoadedVm;
use mesh::error::RemoteError;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::rpc::MicrovmHostPauseError;
use openvmm_defs::rpc::MicrovmRunState;
use openvmm_defs::rpc::MicrovmRunStatus;

/// Host pause state of a [`LoadedVm`].
#[derive(Default)]
pub(super) struct HostPause {
    /// Whether the host holds the VM paused.
    paused: bool,
    /// Guest time held by the current host pause, unless a reset discarded it.
    held: Option<HeldGuestTime>,
    /// How many times the VM entered or left the host-paused state.
    transitions: u64,
}

/// Guest time held while the host has paused the VM.
struct HeldGuestTime {
    /// VP 0's TSC when the vCPUs stopped.
    #[cfg(guest_arch = "x86_64")]
    tsc: u64,
    /// Every VP's LAPIC state when the vCPUs stopped, in VP order.
    #[cfg(guest_arch = "x86_64")]
    lapics: Vec<virt::x86::vp::Apic>,
}

impl HostPause {
    /// Records a completed host pause that holds `held`.
    fn hold(&mut self, held: HeldGuestTime) {
        self.paused = true;
        self.held = Some(held);
        self.transitions += 1;
    }

    /// Discards the held guest time before a reset reinitializes the
    /// partition. The host pause stays in place.
    pub(super) fn discard_held_time(&mut self) {
        self.held = None;
    }

    /// Ends the host pause after a failed reset, so that a host resume cannot
    /// start the partially reset VM.
    pub(super) fn abandon(&mut self) {
        self.held = None;
        if std::mem::take(&mut self.paused) {
            self.transitions += 1;
        }
    }

    /// Records that the VM runs again. Returns whether this released a host
    /// pause.
    fn release(&mut self) -> bool {
        if !std::mem::take(&mut self.paused) {
            return false;
        }
        self.held = None;
        self.transitions += 1;
        true
    }

    /// The run status of a VM that is `running`, or that a snapshot boundary
    /// or restore gate `blocked`.
    fn status(&self, running: bool, blocked: bool) -> MicrovmRunStatus {
        let state = if blocked {
            MicrovmRunState::Busy
        } else if running {
            MicrovmRunState::Running
        } else if self.paused {
            MicrovmRunState::Paused
        } else {
            MicrovmRunState::Stopped
        };
        MicrovmRunStatus {
            state,
            transitions: self.transitions,
        }
    }
}

impl LoadedVm {
    /// Handles [`openvmm_defs::rpc::VmRpc::MicrovmPause`]. Returns `false` if
    /// the host already paused the VM.
    pub(super) async fn microvm_pause(&mut self) -> Result<bool, MicrovmHostPauseError> {
        if self.inner.machine_profile != MachineProfile::Microvm {
            return Err(rejected(anyhow::anyhow!(
                "host pause requires the microVM profile"
            )));
        }
        if self.host_pause_blocked() {
            return Err(MicrovmHostPauseError::Busy);
        }
        if !self.running {
            return if self.host_pause.paused {
                Ok(false)
            } else {
                Err(rejected(anyhow::anyhow!("the VM is not running")))
            };
        }

        self.state_units.stop().await;
        self.running = false;
        match self.hold_guest_time().await {
            Ok(held) => {
                self.host_pause.hold(held);
                tracing::info!(
                    transitions = self.host_pause.transitions,
                    "microVM paused by the host"
                );
                Ok(true)
            }
            Err(error) => {
                // Nothing is held, so the guest only observes a brief stall.
                if let Err(start_error) = self.start_state_units().await {
                    tracing::error!(
                        error = format!("{error:#}"),
                        start_error = format!("{start_error:#}"),
                        "microVM host pause failed and the VM could not start again"
                    );
                    return Err(MicrovmHostPauseError::Uncertain(RemoteError::new(
                        start_error.context(format!("host pause failed: {error:#}")),
                    )));
                }
                self.running = true;
                tracing::warn!(
                    error = format!("{error:#}"),
                    "microVM host pause rejected; the guest continues"
                );
                Err(rejected(error))
            }
        }
    }

    /// Handles [`openvmm_defs::rpc::VmRpc::MicrovmResume`]: resumes only a VM
    /// that the host paused. Returns `false` if the VM is already running.
    pub(super) async fn microvm_resume(&mut self) -> Result<bool, MicrovmHostPauseError> {
        if self.host_pause_blocked() {
            return Err(MicrovmHostPauseError::Busy);
        }
        if self.running {
            return Ok(false);
        }
        if !self.host_pause.paused {
            return Err(rejected(anyhow::anyhow!(
                "the VM is stopped and was not paused by the host"
            )));
        }
        // The held time stays in place on failure, so the host can retry.
        self.restore_held_guest_time().await.map_err(|error| {
            rejected(error.context("failed to restore the guest time held by the host pause"))
        })?;
        if let Err(error) = self.start_state_units().await {
            tracing::error!(
                error = format!("{error:#}"),
                "microVM host resume could not start the VM"
            );
            return Err(MicrovmHostPauseError::Uncertain(RemoteError::new(error)));
        }
        self.running = true;
        self.complete_host_resume();
        Ok(true)
    }

    /// Restores the guest time held by a host pause, before the vCPUs run
    /// again. The held time stays in place until the VM is running, so a
    /// failed resume can be retried.
    pub(super) async fn restore_held_guest_time(&mut self) -> anyhow::Result<()> {
        let Some(held) = &self.host_pause.held else {
            return Ok(());
        };
        #[cfg(guest_arch = "x86_64")]
        {
            let state = self
                .inner
                .time_abi
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("host pause requires the time ABI"))?;
            super::time_abi::set_held_tsc(self.inner.partition.as_ref(), state, held.tsc)?;
            self.inner
                .partition_unit
                .set_lapics(held.lapics.clone())
                .await?;
        }
        #[cfg(not(guest_arch = "x86_64"))]
        let _ = held;
        Ok(())
    }

    /// Records that the VM is running again, releasing any host pause.
    pub(super) fn complete_host_resume(&mut self) {
        if self.host_pause.release() {
            tracing::info!(
                transitions = self.host_pause.transitions,
                "microVM resumed by the host"
            );
        }
    }

    /// Handles [`openvmm_defs::rpc::VmRpc::MicrovmRunState`].
    pub(super) fn microvm_run_status(&self) -> MicrovmRunStatus {
        self.host_pause
            .status(self.running, self.host_pause_blocked())
    }

    /// Returns whether a snapshot boundary or post-restore gate is active.
    fn host_pause_blocked(&self) -> bool {
        self.snapshot_boundary.is_active() || self.snapshot_restore.gate_timeout.is_some()
    }

    /// Takes the guest time to hold with every vCPU stopped: checks that the
    /// LAPIC timers are one-shot, takes VP 0's TSC, and reads every LAPIC.
    #[cfg(guest_arch = "x86_64")]
    async fn hold_guest_time(&mut self) -> anyhow::Result<HeldGuestTime> {
        let state = self
            .inner
            .time_abi
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("host pause requires the time ABI"))?;
        let tsc = super::time_abi::held_tsc(self.inner.partition.as_ref(), state)?;
        self.inner.partition_unit.check_one_shot_timers().await?;
        let lapics = self.inner.partition_unit.get_lapics().await?;
        Ok(HeldGuestTime { tsc, lapics })
    }

    #[cfg(not(guest_arch = "x86_64"))]
    async fn hold_guest_time(&mut self) -> anyhow::Result<HeldGuestTime> {
        anyhow::bail!("host pause requires an x86-64 guest")
    }
}

fn rejected(error: anyhow::Error) -> MicrovmHostPauseError {
    MicrovmHostPauseError::Rejected(RemoteError::new(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn held() -> HeldGuestTime {
        HeldGuestTime {
            #[cfg(guest_arch = "x86_64")]
            tsc: 1,
            #[cfg(guest_arch = "x86_64")]
            lapics: Vec::new(),
        }
    }

    fn status(state: MicrovmRunState, transitions: u64) -> MicrovmRunStatus {
        MicrovmRunStatus { state, transitions }
    }

    #[test]
    fn pause_and_resume_count_transitions() {
        let mut pause = HostPause::default();
        assert_eq!(
            pause.status(true, false),
            status(MicrovmRunState::Running, 0)
        );
        assert_eq!(
            pause.status(false, false),
            status(MicrovmRunState::Stopped, 0)
        );
        // A start that did not release a host pause is not a transition.
        assert!(!pause.release());

        pause.hold(held());
        assert_eq!(
            pause.status(false, false),
            status(MicrovmRunState::Paused, 1)
        );
        assert!(pause.release());
        assert!(pause.held.is_none());
        assert_eq!(
            pause.status(true, false),
            status(MicrovmRunState::Running, 2)
        );
        assert!(!pause.release());
        assert_eq!(pause.transitions, 2);
    }

    #[test]
    fn reset_discards_held_time_but_keeps_the_pause() {
        let mut pause = HostPause::default();
        pause.hold(held());
        pause.discard_held_time();
        assert!(pause.held.is_none());
        assert_eq!(
            pause.status(false, false),
            status(MicrovmRunState::Paused, 1)
        );
        assert!(pause.release());
        assert_eq!(pause.transitions, 2);

        // Without a host pause, a reset leaves a stopped VM stopped.
        pause.discard_held_time();
        assert_eq!(
            pause.status(false, false),
            status(MicrovmRunState::Stopped, 2)
        );
    }

    #[test]
    fn failed_reset_ends_the_pause() {
        let mut pause = HostPause::default();
        pause.hold(held());
        pause.abandon();
        assert!(pause.held.is_none());
        assert_eq!(
            pause.status(false, false),
            status(MicrovmRunState::Stopped, 2)
        );
        // A later resume releases nothing.
        assert!(!pause.release());

        // Without a host pause, a failed reset is not a transition.
        pause.abandon();
        assert_eq!(pause.transitions, 2);
    }

    #[test]
    fn snapshot_boundary_or_restore_gate_reports_busy() {
        let mut pause = HostPause::default();
        assert_eq!(pause.status(true, true), status(MicrovmRunState::Busy, 0));
        pause.hold(held());
        assert_eq!(pause.status(false, true), status(MicrovmRunState::Busy, 1));
    }
}
