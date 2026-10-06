// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile support for the OpenVMM entry point.

mod config;
mod console;
mod filesystem;
mod host_control;
mod launch;
mod network;
pub(crate) mod output;
pub(crate) mod report;
mod restore;

pub(crate) use config::MicrovmConfigBuilder;
#[cfg(test)]
pub(crate) use console::MICROVM_CONSOLE_ATTACHMENT_KIND;
#[cfg(test)]
pub(crate) use console::MICROVM_CONSOLE_STABLE_ID;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_STABLE_ID;
pub(crate) use console::MicrovmConsoleSocketCleanup;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_attachment_from_cli;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_attachment_from_snapshot;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_socket_cleanup;
pub(crate) use console::microvm_control_authentication_from_capability;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::validate_microvm_console_attachment_namespace;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_attachment;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_from_snapshot;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_slot_from_snapshot;
pub(crate) use filesystem::validate_microvm_filesystem_private_storage;
pub(crate) use host_control::MicrovmHostControlServer;
pub(crate) use launch::MicrovmLaunch;
pub(crate) use restore::ExpectedRestoreContract;
pub(crate) use restore::MicrovmRestore;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use restore::fresh_microvm_generation_id;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use restore::fresh_microvm_restore_packet;
pub(crate) use restore::prepare_restore;
pub(crate) use restore::validate_restore_contract;

use crate::storage_builder::microvm::MicrovmSandboxBlockSource;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use net_backend_resources::egress::EgressPolicy;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use output::MicrovmOutputDrain;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Host-side microVM resources produced while building the VM configuration
/// and consumed by the snapshot, restore, and teardown paths.
#[derive(Default)]
pub(crate) struct MicrovmResources {
    /// Drains portb and its host output relay before a guest-requested exit.
    pub(crate) output_drain: Option<MicrovmOutputDrain>,
    /// Guest-requested snapshot boundaries from the snapshot-request port.
    pub(crate) snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
    /// Snapshot sources of the fixed-role sandbox blocks, in role order.
    pub(crate) sandbox_block_sources: Vec<MicrovmSandboxBlockSource>,
    /// Host control channels for active image slots; inactive entries are absent.
    pub(crate) image_slot_requests:
        Vec<Option<mesh::Sender<virtio_resources::blk::ImageSlotRequest>>>,
    /// Excludes image binding while capture or another lifecycle transition begins.
    pub(crate) image_slot_transition: Arc<AtomicBool>,
    /// Authenticated versioned host-control service.
    pub(crate) host_control: Option<Arc<futures::lock::Mutex<MicrovmHostControlServer>>>,
    /// Removes the host-control Unix socket on teardown.
    pub(crate) host_control_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the boot virtio-console endpoint.
    pub(crate) console_attachment: Option<SnapshotAttachment>,
    /// Removes the boot console Unix socket on teardown.
    pub(crate) console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the control virtio-console endpoint.
    pub(crate) control_console_attachment: Option<SnapshotAttachment>,
    /// Removes the control console Unix socket on teardown.
    pub(crate) control_console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the portable network attachment.
    pub(crate) network_attachment: Option<SnapshotAttachment>,
    /// The bound run-scoped egress policy.
    pub(crate) egress_policy: Option<EgressPolicy>,
    /// Snapshot identity of the live filesystem root.
    pub(crate) filesystem_attachment: Option<SnapshotAttachment>,
    /// Canonical host path of the live filesystem root.
    pub(crate) filesystem_root_path: Option<PathBuf>,
}
