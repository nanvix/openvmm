// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for microVM chipset devices.

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::SerialBackendHandle;

/// Scratch handling requested at a microVM snapshot boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub enum MicrovmSnapshotScratchPolicy {
    /// Scratch is unmounted and restore must attach a fresh image.
    Fresh,
    /// Scratch is mounted and must be paired with VM state.
    Paired,
}

/// Worker-local request used to establish the exact post-OUT snapshot boundary.
#[derive(MeshPayload)]
pub struct MicrovmSnapshotBoundaryRequest {
    /// Whether capture must pair the current scratch image.
    pub scratch_policy: MicrovmSnapshotScratchPolicy,
    /// Signals the device after all vCPUs have stopped.
    pub release_write: mesh::OneshotSender<()>,
    /// Completes after the deferred PMIO write has completed.
    pub write_completed: mesh::OneshotReceiver<()>,
    /// Maximum time allowed to gate host input before stopping vCPUs.
    pub input_gate_timeout: std::time::Duration,
    /// Completed only after the final transaction outcome is known, with the
    /// reason that the VMM continued the guest without a capture, if any.
    pub transaction_complete: mesh::rpc::Rpc<(), Option<MicrovmSnapshotRejection>>,
}

/// Reason that a guest-requested microVM snapshot was not captured and the
/// guest continued. The snapshot-request port reports it to the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload)]
pub enum MicrovmSnapshotRejection {
    /// A vCPU had an armed periodic LAPIC timer. Restore would coalesce the
    /// periods that the timer missed during the snapshot downtime into one
    /// interrupt, so a guest tick counted from them would fall behind.
    PeriodicLapicTimer,
    /// Capture was rejected or rolled back for any other reason.
    Other,
}

/// The microVM bidirectional portb console at ports `0xe9` and `0xea`.
#[derive(MeshPayload)]
pub struct MicrovmPortbHandle {
    /// Host serial endpoint used for raw input and output.
    pub io: Resource<SerialBackendHandle>,
    /// Fresh generation ID for this microVM instance.
    pub generation_id: [u8; 16],
    /// Fresh entropy exposed only through the private restore-input selector.
    pub restore_entropy: Vec<u8>,
    /// Requests to drain and close output before terminating the VM process.
    pub output_drain: Option<mesh::Receiver<mesh::rpc::FailableRpc<(), ()>>>,
}

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmPortbHandle {
    const ID: &'static str = "microvm-portb";
}

/// microVM shutdown control port at `0x604`.
#[derive(MeshPayload)]
pub struct MicrovmShutdownHandle;

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmShutdownHandle {
    const ID: &'static str = "microvm-shutdown";
}

/// microVM snapshot-request port at `0x605`.
///
/// Writes request a capture. Reads return the outcome of the last completed
/// request in the first byte: `0` when it was not rejected, `1` for
/// [`MicrovmSnapshotRejection::PeriodicLapicTimer`], and `2` for
/// [`MicrovmSnapshotRejection::Other`]. A new or restored VM reads `0`.
#[derive(MeshPayload)]
pub struct MicrovmSnapshotRequestHandle {
    /// Optional worker-local boundary coordination target.
    pub notify: Option<mesh::Sender<MicrovmSnapshotBoundaryRequest>>,
    /// Maximum time allowed to gate host input before stopping vCPUs.
    pub input_gate_timeout: std::time::Duration,
}

impl ResourceId<ChipsetDeviceHandleKind> for MicrovmSnapshotRequestHandle {
    const ID: &'static str = "microvm-snapshot-request";
}
