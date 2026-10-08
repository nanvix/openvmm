// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RPC types for communicating with the VM worker.

use crate::config::DeviceVtl;
use guid::Guid;
use mesh::CancelContext;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::payload::message::ProtobufMessage;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use std::fmt;
use std::fs::File;
use std::time::Duration;
use vm_resource::Resource;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;

#[derive(MeshPayload)]
pub enum VmRpc {
    Save(FailableRpc<(), ProtobufMessage>),
    /// Boundedly quiesce the VM and return saved state while leaving it stopped.
    QuiesceForSnapshot(Rpc<Duration, Result<SnapshotSaveResponse, SnapshotQuiesceError>>),
    /// Resume a VM after a rollback-safe snapshot failure before commit.
    ResumeAfterFailedSnapshot(FailableRpc<Duration, ()>),
    /// Release a post-OUT boundary without starting a snapshot transaction.
    ReleaseSnapshotBoundary(FailableRpc<(), ()>),
    Resume(FailableRpc<(), bool>),
    Pause(Rpc<(), bool>),
    /// Pause a running microVM for its host while holding guest time, so the
    /// guest observes no elapsed time across the pause. Returns `false` if the
    /// host already paused it.
    MicrovmPause(Rpc<(), Result<bool, MicrovmHostPauseError>>),
    /// Resume a microVM that its host paused, restoring the held guest time.
    /// Returns `false` if the VM is already running.
    MicrovmResume(Rpc<(), Result<bool, MicrovmHostPauseError>>),
    /// Report the host-visible run state of the VM.
    MicrovmRunState(Rpc<(), MicrovmRunStatus>),
    ClearHalt(Rpc<(), bool>),
    Reset(FailableRpc<(), ()>),
    Nmi(Rpc<u32, ()>),
    AddVmbusDevice(FailableRpc<(DeviceVtl, Resource<VmbusDeviceHandleKind>), ()>),
    ConnectHvsock(FailableRpc<(CancelContext, Guid, DeviceVtl), unix_socket::UnixStream>),
    PulseSaveRestore(Rpc<(), Result<(), PulseSaveRestoreError>>),
    StartReloadIgvm(FailableRpc<File, ()>),
    CompleteReloadIgvm(FailableRpc<bool, ()>),
    ReadMemory(FailableRpc<(u64, usize), Vec<u8>>),
    WriteMemory(FailableRpc<(u64, Vec<u8>), ()>),
    /// Updates the command line parameters that will be passed to the boot shim
    /// on the *next* VM load. This will replace the existing command line parameters.
    UpdateCliParams(FailableRpc<String, ()>),
    /// Hot-add a PCIe device to a named port at runtime.
    /// Tuple is (port_name, device_resource).
    AddPcieDevice(FailableRpc<(String, Resource<PciDeviceHandleKind>), ()>),
    /// Hot-remove a PCIe device from a named port at runtime.
    RemovePcieDevice(FailableRpc<String, ()>),
    /// Hot-add a VPCI device to VTL0 at runtime with the supplied instance ID.
    AddVpciDevice(FailableRpc<(Guid, Resource<PciDeviceHandleKind>), ()>),
    /// Hot-remove a dynamically added VPCI device by instance ID.
    RemoveVpciDevice(FailableRpc<Guid, ()>),
    /// Dump VM state (VP registers + memory) to a `.vmrs` file.
    ///
    /// The worker pauses the VM internally, collects state, and restores
    /// the prior running state afterward. The caller provides an open file
    /// handle to write to (typically a temporary file that gets renamed
    /// into place on success).
    DumpState(FailableRpc<File, ()>),
}

/// State returned after a successful bounded snapshot quiesce.
#[derive(Debug, MeshPayload)]
pub struct SnapshotSaveResponse {
    /// Encoded VM saved state.
    pub saved_state: ProtobufMessage,
    /// Complete state-unit inventory in stable registration order.
    pub state_unit_names: Vec<String>,
    /// Effective Linux direct command line loaded into the guest.
    pub effective_command_line: String,
    /// The NVX time ABI records of the capture, which make the manifest
    /// version 6.
    pub time: crate::time_abi::TimeCapture,
}

/// Failure classification for a bounded snapshot quiesce/save operation.
#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum SnapshotQuiesceError {
    /// The request was rejected before any state transition began.
    #[error("snapshot quiesce request was rejected")]
    Rejected(#[source] RemoteError),
    /// No uncertain transition occurred; the controller may request rollback.
    #[error("snapshot quiesce failed without uncertain state")]
    RollbackSafe(#[source] RemoteError),
    /// A unit may have partially transitioned; the VM must be terminated.
    #[error("snapshot quiesce left uncertain state")]
    Uncertain(#[source] RemoteError),
}

/// Host-visible run state of a microVM.
#[derive(Debug, Copy, Clone, PartialEq, Eq, MeshPayload)]
pub enum MicrovmRunState {
    /// The guest is running.
    Running,
    /// The host paused the guest with [`VmRpc::MicrovmPause`] and holds its
    /// time until [`VmRpc::MicrovmResume`].
    Paused,
    /// The VM is stopped for another reason, such as before its first start.
    Stopped,
    /// A snapshot boundary or post-restore gate is active. Host pause is
    /// unavailable until it ends.
    Busy,
}

/// The run state of a VM and the number of times it entered or left the
/// host-paused state since it was launched.
#[derive(Debug, Copy, Clone, PartialEq, Eq, MeshPayload)]
pub struct MicrovmRunStatus {
    /// The run state.
    pub state: MicrovmRunState,
    /// How many times the VM entered or left the host-paused state. It is odd
    /// exactly while the host holds a pause.
    pub transitions: u64,
}

/// Failure of a [`VmRpc::MicrovmPause`] or [`VmRpc::MicrovmResume`] request.
#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum MicrovmHostPauseError {
    /// A snapshot boundary or post-restore gate is active. The VM is unchanged.
    #[error("host pause is unavailable while a snapshot boundary or restore gate is active")]
    Busy,
    /// The request was rejected and the VM keeps its run state, apart from a
    /// brief vCPU stop when a pause is rejected after the vCPUs stopped.
    #[error("the request was rejected and the VM keeps its run state")]
    Rejected(#[source] RemoteError),
    /// The VM could not start again. Its state is uncertain and it must be
    /// torn down.
    #[error("the VM could not start again and its state is uncertain")]
    Uncertain(#[source] RemoteError),
}

#[derive(Debug, MeshPayload, thiserror::Error)]
pub enum PulseSaveRestoreError {
    #[error("reset not supported")]
    ResetNotSupported,
    #[error("pulse save+restore failed")]
    Other(#[source] RemoteError),
    #[error("save and restore are unavailable for this machine profile")]
    UnsupportedMachineProfile,
}

impl From<anyhow::Error> for PulseSaveRestoreError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(RemoteError::new(err))
    }
}

impl fmt::Debug for VmRpc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            VmRpc::Reset(_) => "Reset",
            VmRpc::Save(_) => "Save",
            VmRpc::QuiesceForSnapshot(_) => "QuiesceForSnapshot",
            VmRpc::ResumeAfterFailedSnapshot(_) => "ResumeAfterFailedSnapshot",
            VmRpc::ReleaseSnapshotBoundary(_) => "ReleaseSnapshotBoundary",
            VmRpc::Resume(_) => "Resume",
            VmRpc::Pause(_) => "Pause",
            VmRpc::MicrovmPause(_) => "MicrovmPause",
            VmRpc::MicrovmResume(_) => "MicrovmResume",
            VmRpc::MicrovmRunState(_) => "MicrovmRunState",
            VmRpc::ClearHalt(_) => "ClearHalt",
            VmRpc::Nmi(_) => "Nmi",
            VmRpc::AddVmbusDevice(_) => "AddVmbusDevice",
            VmRpc::ConnectHvsock(_) => "ConnectHvsock",
            VmRpc::PulseSaveRestore(_) => "PulseSaveRestore",
            VmRpc::StartReloadIgvm(_) => "StartReloadIgvm",
            VmRpc::CompleteReloadIgvm(_) => "CompleteReloadIgvm",
            VmRpc::ReadMemory(_) => "ReadMemory",
            VmRpc::WriteMemory(_) => "WriteMemory",
            VmRpc::UpdateCliParams(_) => "UpdateCliParams",
            VmRpc::AddPcieDevice(_) => "AddPcieDevice",
            VmRpc::RemovePcieDevice(_) => "RemovePcieDevice",
            VmRpc::AddVpciDevice(_) => "AddVpciDevice",
            VmRpc::RemoveVpciDevice(_) => "RemoveVpciDevice",
            VmRpc::DumpState(_) => "DumpState",
        };
        f.pad(s)
    }
}
