// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM snapshot capture, guest exit, and teardown handling of the VM controller.

use super::VmController;
use super::VmControllerEvent;
use crate::microvm::MicrovmResources;
use anyhow::Context;
use mesh::rpc::RpcSend;
use openvmm_defs::rpc::SnapshotQuiesceError;
use openvmm_defs::rpc::VmRpc;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// MicroVM state owned by the VM controller.
#[derive(Default)]
pub(crate) struct MicrovmController {
    /// Whether the microVM machine profile is selected.
    pub(crate) active: bool,
    /// Exact open handle of the file backing guest RAM for snapshot capture.
    pub(crate) snapshot_memory_handle: Option<std::fs::File>,
    /// Immutable RAM capacity reserved in captured snapshots.
    pub(crate) memory_capacity: Option<u64>,
    /// Guest-requested snapshot boundaries released by the VM worker.
    pub(crate) snapshot_requests:
        Option<mesh::Receiver<chipset_resources::microvm::MicrovmSnapshotScratchPolicy>>,
    /// Directory receiving a guest-requested snapshot.
    pub(crate) snapshot_destination: Option<PathBuf>,
    /// Sandbox capture tier of a guest-requested snapshot.
    pub(crate) snapshot_tier: Option<crate::cli_args::microvm::SnapshotTierCli>,
    /// Sandbox-block identity policy recorded in a captured snapshot.
    pub(crate) snapshot_block_identity: crate::cli_args::microvm::SnapshotBlockIdentityCli,
    /// Caller-authenticated immutable storage generation.
    pub(crate) snapshot_generation_id: Option<[u8; 16]>,
    /// Restore-time materialization policy recorded for paired scratch.
    pub(crate) snapshot_scratch_restore_mode:
        crate::cli_args::microvm::SnapshotScratchRestoreModeCli,
    /// Maximum time allowed to quiesce the VM for a guest-requested snapshot.
    pub(crate) snapshot_quiesce_timeout: Duration,
    /// Hypervisor backend recorded in captured machine contracts.
    pub(crate) source_hypervisor: String,
    /// Effective kernel command line of the MP-table boot.
    pub(crate) effective_command_line: Option<String>,
    /// Host attachments and cleanup guards of the microVM devices.
    pub(crate) resources: MicrovmResources,
    /// Static identity of the microVM virtio-net device.
    pub(crate) network: Option<openvmm_defs::microvm::MicrovmNetworkConfig>,
    /// Whether the first microVM virtio-fs slot is present.
    pub(crate) filesystem_slot: bool,
    /// Guest-visible policies of the active microVM filesystems, in virtio-fs
    /// slot order.
    pub(crate) filesystems: Vec<openvmm_defs::microvm::MicrovmFilesystemConfig>,
    /// Fixed image-slot activation contract.
    pub(crate) image_slots: Option<openvmm_defs::microvm::MicrovmImageSlotsConfig>,
    /// Automatic RAM backing created for snapshot capture.
    pub(crate) snapshot_memory_file: Option<tempfile::NamedTempFile>,
    /// Private copy of a paired scratch image, kept alive for the VM lifetime.
    pub(crate) _private_scratch_dir: Option<tempfile::TempDir>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MicrovmTeardownStatus {
    pub(crate) vm_worker_stopped: bool,
    pub(crate) auxiliary_workers_stopped: bool,
}

impl MicrovmTeardownStatus {
    pub(crate) fn complete(self) -> bool {
        self.vm_worker_stopped && self.auxiliary_workers_stopped
    }

    /// Fails a run whose workers did not stop cleanly when `enforce` is set.
    pub(crate) fn enforce(self, enforce: bool, result: anyhow::Result<i32>) -> anyhow::Result<i32> {
        if enforce && !self.complete() {
            let teardown_error = MicrovmTeardownError(self);
            return match result {
                Ok(_) => Err(teardown_error.into()),
                Err(error) => Err(error.context(teardown_error)),
            };
        }
        result
    }
}

#[derive(Debug)]
pub(crate) struct MicrovmTeardownError(pub(crate) MicrovmTeardownStatus);

impl std::fmt::Display for MicrovmTeardownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("one or more microVM workers failed to stop cleanly")
    }
}

impl std::error::Error for MicrovmTeardownError {}

pub(super) enum GuestSnapshotAction {
    Continue,
    Terminate { exit_code: i32 },
}

struct ImageSlotTransitionGuard(Arc<AtomicBool>);

impl Drop for ImageSlotTransitionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn ensure_image_slot_channels_empty(
    slots: &[Option<mesh::Sender<virtio_resources::blk::ImageSlotRequest>>],
) -> anyhow::Result<()> {
    for (index, requests) in slots.iter().enumerate() {
        let Some(requests) = requests else {
            continue;
        };
        let state = requests
            .call(virtio_resources::blk::ImageSlotRequest::Query, ())
            .await
            .with_context(|| format!("image slot {index} did not answer"))?;
        anyhow::ensure!(
            state.identity.is_none(),
            "image slot {index} is bound to '{}'",
            state.identity.as_deref().unwrap_or_default()
        );
    }
    Ok(())
}

async fn guest_exit_event(
    code: i32,
    drain: Option<crate::microvm::output::MicrovmOutputDrain>,
) -> VmControllerEvent {
    if let Some(drain) = drain
        && let Err(error) = drain.drain().await
    {
        tracing::error!(
            error = error.as_ref() as &dyn std::error::Error,
            "failed to drain microVM console output before exit"
        );
        return VmControllerEvent::ExitFailed {
            error: format!("failed to drain microVM console output: {error:#}"),
        };
    }
    VmControllerEvent::ExitRequested { code }
}

impl VmController {
    pub(super) async fn request_exit(
        &mut self,
        code: i32,
        events: &mesh::Sender<VmControllerEvent>,
    ) {
        events.send(guest_exit_event(code, self.microvm.resources.output_drain.take()).await);
    }

    pub(super) async fn handle_guest_snapshot_request(
        &mut self,
        scratch_policy: chipset_resources::microvm::MicrovmSnapshotScratchPolicy,
    ) -> GuestSnapshotAction {
        let _image_slot_guard = if self.microvm.image_slots.is_some() {
            let transition = self.microvm.resources.image_slot_transition.clone();
            if transition
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                tracelimit::warn_ratelimited!(
                    "ignoring microVM snapshot request because an image-slot transition is active"
                );
                return self.release_snapshot_boundary_without_capture().await;
            }
            Some(ImageSlotTransitionGuard(transition))
        } else {
            None
        };
        let Some(destination) = self.microvm.snapshot_destination.clone() else {
            tracelimit::warn_ratelimited!(
                "ignoring microVM snapshot request because no destination is configured"
            );
            return self.release_snapshot_boundary_without_capture().await;
        };
        if let Err(error) = self.ensure_image_slots_empty().await {
            tracing::error!(
                error = error.as_ref() as &dyn std::error::Error,
                "microVM snapshot preflight rejected bound image slots; guest continues"
            );
            return self.release_snapshot_boundary_without_capture().await;
        }

        let preflight = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                self.microvm.active,
                "guest-requested snapshot capture requires the microVM profile"
            );
            anyhow::ensure!(
                matches!(
                    self.microvm.source_hypervisor.as_str(),
                    "kvm" | "mshv" | "whp"
                ),
                "microVM snapshot source backend must be KVM, MSHV, or WHP"
            );
            anyhow::ensure!(
                self.microvm.resources.sandbox_block_sources.is_empty()
                    || (self.microvm.resources.sandbox_block_sources.len() >= 2
                        && self
                            .microvm
                            .resources
                            .sandbox_block_sources
                            .last()
                            .is_some_and(|source| {
                                source.role
                                    == openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch
                            })),
                "microVM snapshot requires either no blocks or at least one lower layer and scratch"
            );
            if self.microvm.resources.sandbox_block_sources.is_empty() {
                anyhow::ensure!(
                    self.microvm.snapshot_tier.is_none(),
                    "blockless microVM snapshot capture does not use a tier"
                );
            } else {
                let tier = self
                    .microvm
                    .snapshot_tier
                    .context("microVM sandbox snapshot capture requires a tier")?;
                let paired_scratch = scratch_policy
                    == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired;
                anyhow::ensure!(
                    paired_scratch == tier.requires_paired_scratch(),
                    "snapshot tier '{}' requires {} scratch capture",
                    tier.manifest_name(),
                    if tier.requires_paired_scratch() {
                        "paired"
                    } else {
                        "fresh"
                    }
                );
            }
            anyhow::ensure!(
                fs_err::symlink_metadata(&destination)
                    .is_err_and(|error| { error.kind() == std::io::ErrorKind::NotFound }),
                "snapshot destination already exists or cannot be inspected: {}",
                destination.display()
            );
            let memory_path = self
                .memory_backing_file
                .clone()
                .context("microVM snapshot capture requires file-backed RAM")?;
            let memory_file = self
                .microvm
                .snapshot_memory_handle
                .as_ref()
                .context("microVM snapshot capture lost its exact RAM handle")?;
            anyhow::ensure!(
                memory_file
                    .metadata()
                    .context("failed to inspect snapshot RAM handle")?
                    .len()
                    == self.memory,
                "snapshot RAM handle size does not match the VM"
            );
            if let Some(file) = &self.microvm.snapshot_memory_file {
                anyhow::ensure!(
                    file.path() == memory_path,
                    "automatic snapshot memory backing path changed unexpectedly"
                );
            }
            anyhow::ensure!(
                self.microvm.effective_command_line.is_some(),
                "microVM snapshot capture requires an effective command line"
            );
            Ok(())
        })();
        if let Err(error) = preflight {
            tracing::error!(
                error = error.as_ref() as &dyn std::error::Error,
                "microVM snapshot preflight failed; guest continues"
            );
            return self.release_snapshot_boundary_without_capture().await;
        }

        // The source terminates once the snapshot commits, so deliver the
        // console output the guest wrote before its request while the endpoint
        // is still open. Bytes that miss the bound stay in the snapshot.
        if let Some(drain) = &self.microvm.resources.output_drain {
            let output_flush = openvmm_defs::profile::ProfileSpan::start();
            match drain.flush(self.microvm.snapshot_quiesce_timeout).await {
                Ok(()) => output_flush.complete("capture", "output_flush", Default::default()),
                Err(error) => tracelimit::warn_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    "failed to flush microVM console output before snapshot; undelivered bytes stay in the snapshot"
                ),
            }
        }

        let response = match self
            .vm_rpc
            .call(
                VmRpc::QuiesceForSnapshot,
                self.microvm.snapshot_quiesce_timeout,
            )
            .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(SnapshotQuiesceError::Rejected(error))) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "microVM snapshot request was rejected; guest continues"
                );
                return self.release_snapshot_boundary_without_capture().await;
            }
            Ok(Err(SnapshotQuiesceError::RollbackSafe(error))) => {
                return self.rollback_failed_guest_snapshot(error.into()).await;
            }
            Ok(Err(SnapshotQuiesceError::Uncertain(error))) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "microVM snapshot quiesce left uncertain state; terminating source"
                );
                return GuestSnapshotAction::Terminate { exit_code: 1 };
            }
            Err(error) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "lost VM worker during snapshot quiesce; terminating source"
                );
                return GuestSnapshotAction::Terminate { exit_code: 1 };
            }
        };
        let command_line = response.effective_command_line;

        let result = (|| -> anyhow::Result<()> {
            let network = self
                .microvm
                .network
                .as_ref()
                .zip(self.microvm.resources.egress_policy.as_ref())
                .zip(self.microvm.resources.network_attachment.clone())
                .map(|((network, policy), attachment)| (network, policy, attachment));
            let filesystems = self
                .microvm
                .filesystems
                .iter()
                .zip(&self.microvm.resources.filesystem_root_paths)
                .zip(&self.microvm.resources.filesystem_attachments)
                .map(|((filesystem, root_path), attachment)| {
                    (filesystem, root_path.as_path(), attachment.clone())
                })
                .collect();
            let mut blocks = crate::storage_builder::microvm::capture_snapshot_block_contract(
                &self.microvm.resources.sandbox_block_sources,
                scratch_policy,
                self.microvm.snapshot_block_identity,
                self.microvm.snapshot_generation_id,
                self.microvm.snapshot_scratch_restore_mode.manifest_name(),
            )?;
            if self.microvm.snapshot_tier
                == Some(crate::cli_args::microvm::SnapshotTierCli::Platform)
            {
                for block in blocks.iter_mut().filter(|block| block.read_only) {
                    block.identity_kind = "unbound".to_owned();
                    block.identity.clear();
                }
            }
            let machine_contract =
                openvmm_helpers::snapshot::microvm::microvm_machine_contract_with_image_slots(
                    &self.microvm.source_hypervisor,
                    openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
                    command_line,
                    network,
                    self.microvm.filesystem_slot,
                    filesystems,
                    self.microvm.resources.console_attachment.clone(),
                    self.microvm.resources.control_console_attachment.clone(),
                    blocks,
                    self.microvm.image_slots,
                    self.processors,
                    self.memory,
                    self.microvm.memory_capacity,
                    response.state_unit_names,
                    response.time.time,
                    response.time.cpu_profile,
                )?;
            let manifest = openvmm_helpers::snapshot::SnapshotManifest {
                version: openvmm_helpers::snapshot::MANIFEST_VERSION,
                created_at: std::time::SystemTime::now().into(),
                openvmm_version: env!("CARGO_PKG_VERSION").to_owned(),
                memory_size_bytes: self.memory,
                vp_count: self.processors,
                page_size: crate::system_page_size(),
                architecture: crate::GUEST_ARCH.to_owned(),
                state_size_bytes: 0,
                machine_contract: Some(machine_contract),
                format_magic: openvmm_helpers::snapshot::format::SNAPSHOT_FORMAT_MAGIC.to_vec(),
                saved_state_schema_version:
                    openvmm_helpers::snapshot::format::SAVED_STATE_SCHEMA_VERSION,
                saved_state_root_type: openvmm_helpers::snapshot::format::SAVED_STATE_ROOT_TYPE
                    .to_owned(),
                snapshot_tier: self
                    .microvm
                    .snapshot_tier
                    .map(crate::cli_args::microvm::SnapshotTierCli::manifest_name)
                    .unwrap_or_default()
                    .to_owned(),
                restore_policy: self
                    .microvm
                    .snapshot_tier
                    .map(crate::cli_args::microvm::SnapshotTierCli::restore_policy)
                    .unwrap_or_default()
                    .to_owned(),
                consumed_config_sections: match self.microvm.snapshot_tier {
                    Some(crate::cli_args::microvm::SnapshotTierCli::Platform) => {
                        openvmm_helpers::snapshot::format::SNAPSHOT_CONFIG_INVARIANTS
                    }
                    Some(
                        crate::cli_args::microvm::SnapshotTierCli::WorkloadStart
                        | crate::cli_args::microvm::SnapshotTierCli::InstanceCheckpoint,
                    ) => {
                        openvmm_helpers::snapshot::format::SNAPSHOT_CONFIG_INVARIANTS
                            | openvmm_helpers::snapshot::format::SNAPSHOT_CONFIG_IMAGE_BINDING
                            | openvmm_helpers::snapshot::format::SNAPSHOT_CONFIG_SANDBOX
                    }
                    None => 0,
                },
            };
            let saved_state_bytes = mesh::payload::encode(response.saved_state);
            let memory_file = self
                .microvm
                .snapshot_memory_handle
                .as_ref()
                .context("microVM snapshot capture lost its exact RAM handle")?;
            let memory_handle_flush = openvmm_defs::profile::ProfileSpan::start();
            memory_file
                .sync_all()
                .context("failed to flush snapshot RAM handle")?;
            memory_handle_flush.complete(
                "capture",
                "memory_handle_flush",
                openvmm_defs::profile::ProfileCounters {
                    logical_bytes: Some(self.memory),
                    ..Default::default()
                },
            );
            let scratch_file = (!self.microvm.resources.sandbox_block_sources.is_empty()
                && scratch_policy
                    == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired)
                .then(|| {
                    self.microvm
                        .resources
                        .sandbox_block_sources
                        .iter()
                        .find(|source| {
                            source.role == openvmm_defs::microvm::MicrovmSandboxBlockRole::Scratch
                        })
                        .map(|source| &source.file)
                        .context("paired snapshot lost its scratch backing handle")
                })
                .transpose()?;
            let publication = openvmm_defs::profile::ProfileSpan::start();
            let write_result = if self.microvm.snapshot_memory_file.is_some() {
                openvmm_helpers::snapshot::publish::write_snapshot_from_owned_memory_and_scratch_files(
                    &destination,
                    &manifest,
                    &saved_state_bytes,
                    memory_file,
                    scratch_file,
                )
            } else {
                openvmm_helpers::snapshot::publish::write_snapshot_from_memory_and_scratch_files(
                    &destination,
                    &manifest,
                    &saved_state_bytes,
                    memory_file,
                    scratch_file,
                )
            };
            if write_result.is_ok() {
                publication.complete_milestone(
                    "capture",
                    "publication",
                    openvmm_defs::profile::ProfileCounters {
                        logical_bytes: Some(self.memory),
                        ..Default::default()
                    },
                );
            }
            write_result.map_err(anyhow::Error::new)
        })();

        match result {
            Ok(()) => {
                if let Some(cleanup) = self.microvm.resources.console_socket_cleanup.take()
                    && let Err(error) = cleanup.remove_if_owned()
                {
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "snapshot committed but the source console socket could not be removed"
                    );
                    return GuestSnapshotAction::Terminate { exit_code: 1 };
                }
                if let Some(cleanup) = self.microvm.resources.control_console_socket_cleanup.take()
                    && let Err(error) = cleanup.remove_if_owned()
                {
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "snapshot committed but the source control console socket could not be removed"
                    );
                    return GuestSnapshotAction::Terminate { exit_code: 1 };
                }
                tracing::info!(
                    path = %destination.display(),
                    "microVM snapshot committed; terminating source process"
                );
                GuestSnapshotAction::Terminate { exit_code: 0 }
            }
            Err(error) => {
                let write_error =
                    error.downcast_ref::<openvmm_helpers::snapshot::publish::SnapshotWriteError>();
                if write_error.is_some_and(|error| error.is_committed()) {
                    if let Some(cleanup) = self.microvm.resources.console_socket_cleanup.take()
                        && let Err(cleanup_error) = cleanup.remove_if_owned()
                    {
                        tracing::error!(
                            error = cleanup_error.as_ref() as &dyn std::error::Error,
                            "committed snapshot console socket could not be removed"
                        );
                    }
                    if let Some(cleanup) = self.microvm.resources.host_control_socket_cleanup.take()
                        && let Err(error) = cleanup.remove_if_owned()
                    {
                        tracing::error!(
                            error = error.as_ref() as &dyn std::error::Error,
                            "committed snapshot host-control socket could not be removed"
                        );
                    }
                    if let Some(cleanup) =
                        self.microvm.resources.control_console_socket_cleanup.take()
                        && let Err(cleanup_error) = cleanup.remove_if_owned()
                    {
                        tracing::error!(
                            error = cleanup_error.as_ref() as &dyn std::error::Error,
                            "committed snapshot control console socket could not be removed"
                        );
                    }
                    if let Some(cleanup) = self.microvm.resources.host_control_socket_cleanup.take()
                        && let Err(cleanup_error) = cleanup.remove_if_owned()
                    {
                        tracing::error!(
                            error = cleanup_error.as_ref() as &dyn std::error::Error,
                            "committed snapshot host-control socket could not be removed"
                        );
                    }
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        path = %destination.display(),
                        "snapshot committed but final durability reporting failed; terminating source"
                    );
                    GuestSnapshotAction::Terminate { exit_code: 1 }
                } else if write_error.is_some_and(|error| !error.is_rollback_safe()) {
                    tracing::error!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "snapshot failed before commit but automatic RAM alias cleanup is uncertain; terminating source"
                    );
                    GuestSnapshotAction::Terminate { exit_code: 1 }
                } else {
                    self.rollback_failed_guest_snapshot(error).await
                }
            }
        }
    }

    async fn ensure_image_slots_empty(&self) -> anyhow::Result<()> {
        ensure_image_slot_channels_empty(&self.microvm.resources.image_slot_requests).await
    }

    async fn rollback_failed_guest_snapshot(
        &mut self,
        error: anyhow::Error,
    ) -> GuestSnapshotAction {
        tracing::error!(
            error = error.as_ref() as &dyn std::error::Error,
            "microVM snapshot failed before commit; attempting rollback"
        );
        match self
            .vm_rpc
            .call_failable(
                VmRpc::ResumeAfterFailedSnapshot,
                self.microvm.snapshot_quiesce_timeout,
            )
            .await
        {
            Ok(()) => {
                tracing::info!("microVM snapshot rollback succeeded; guest resumed");
                GuestSnapshotAction::Continue
            }
            Err(rollback_error) => {
                tracing::error!(
                    error = &rollback_error as &dyn std::error::Error,
                    "microVM snapshot rollback failed; terminating source"
                );
                GuestSnapshotAction::Terminate { exit_code: 1 }
            }
        }
    }

    async fn release_snapshot_boundary_without_capture(&mut self) -> GuestSnapshotAction {
        match self
            .vm_rpc
            .call_failable(VmRpc::ReleaseSnapshotBoundary, ())
            .await
        {
            Ok(()) => GuestSnapshotAction::Continue,
            Err(error) => {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    "failed to release microVM snapshot boundary; terminating source"
                );
                GuestSnapshotAction::Terminate { exit_code: 1 }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::microvm::output::MicrovmOutputDrain;
    use futures::executor::block_on;
    use test_with_tracing::test;

    #[test]
    fn successful_drain_preserves_guest_exit_status() {
        block_on(async {
            for code in [0, 37] {
                let (drain, mut requests) = MicrovmOutputDrain::new(None);
                let (event, ()) = futures::join!(guest_exit_event(code, Some(drain)), async {
                    requests.recv().await.unwrap().complete(Ok(()));
                });
                assert!(matches!(
                    event,
                    VmControllerEvent::ExitRequested { code: actual } if actual == code
                ));
            }
        });
    }

    #[test]
    fn failed_drain_requests_process_exit_not_worker_stopped() {
        let (drain, requests) = MicrovmOutputDrain::new(None);
        drop(requests);
        let event = block_on(guest_exit_event(0, Some(drain)));
        assert!(matches!(
            event,
            VmControllerEvent::ExitFailed { error }
                if error.contains("failed to drain microVM console output")
        ));
    }

    #[test]
    fn bound_image_slot_rejects_capture_preflight() {
        block_on(async {
            let (requests, mut receiver) = mesh::channel();
            let slots = vec![Some(requests)];
            let (result, ()) = futures::join!(ensure_image_slot_channels_empty(&slots), async {
                match receiver.recv().await.unwrap() {
                    virtio_resources::blk::ImageSlotRequest::Query(rpc) => {
                        rpc.handle_sync(|()| virtio_resources::blk::ImageSlotState {
                            identity: Some("image-a".to_owned()),
                            capacity_sectors: 8,
                        });
                    }
                    virtio_resources::blk::ImageSlotRequest::Bind(_) => {
                        panic!("capture preflight must only query image slots")
                    }
                }
            });
            assert!(result.unwrap_err().to_string().contains("is bound"));
        });
    }
}
