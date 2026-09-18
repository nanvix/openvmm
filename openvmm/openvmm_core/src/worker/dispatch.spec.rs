// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Human-owned open specification vocabulary for `dispatch.rs` snapshot restore.
//
// This module contains no executable restore implementation. Its abstract
// Views describe the pre/post-state of `LoadedVm::restore_snapshot_state`.

use vstd::prelude::*;

verus! {

pub struct RamRegionView {
    pub base_gpa: nat,
    pub bytes: Seq<u8>,
}

pub struct RamView {
    pub regions: Seq<RamRegionView>,
}

pub struct ResourceId {
    pub value: int,
}

pub struct ResourceIdentity {
    pub value: int,
}

pub struct ExternalResourcesView {
    pub attachments: Map<ResourceId, ResourceIdentity>,
}

// Host-managed runtime state may be created or changed while restore runs.
// It is part of the complete VM View so that concurrency and lifecycle
// specifications can observe it, but it is not snapshot state.
pub struct HostOperationalStateView {
    pub value: int,
}

pub struct CompatibilityClass {
    pub value: int,
}

pub struct VpStateView {
    pub value: int,
}

pub struct PartitionStateView {
    pub value: int,
}

pub struct ComponentId {
    pub value: int,
}

pub struct ComponentStateView {
    pub value: int,
}

pub struct VirtualTimeView {
    pub vm_time_100ns: u64,
    pub elapsed_since_snapshot_ns: nat,
}

pub struct VmStateView {
    pub memory: RamView,
    pub compatibility: CompatibilityClass,
    pub vp_capacity: nat,
    pub partition_state: PartitionStateView,
    pub vp_states: Map<nat, VpStateView>,
    pub component_inventory: Set<ComponentId>,
    pub active_component_state: Map<ComponentId, ComponentStateView>,
    pub pending_component_state: Map<ComponentId, ComponentStateView>,
    pub virtual_time: VirtualTimeView,
    pub resources: ExternalResourcesView,
    pub host_operational_state: HostOperationalStateView,
}

// The stable, guest-relevant state represented by snapshot artifacts. This
// excludes host operational details such as tasks, sockets, wakers, and host
// queue occupancy.
pub struct SnapshotVmStateView {
    pub memory: RamView,
    pub partition_state: PartitionStateView,
    pub vp_states: Map<nat, VpStateView>,
    pub component_inventory: Set<ComponentId>,
    pub active_component_state: Map<ComponentId, ComponentStateView>,
    pub pending_component_state: Map<ComponentId, ComponentStateView>,
    pub virtual_time: VirtualTimeView,
}

pub struct InitializedVmView {
    pub state: VmStateView,
    pub boot_online_vps: nat,
}

pub struct SavedVmStateView {
    pub partition_state: PartitionStateView,
    pub vp_states: Map<nat, VpStateView>,
    pub component_inventory: Set<ComponentId>,
    pub active_component_state: Map<ComponentId, ComponentStateView>,
    pub pending_component_state: Map<ComponentId, ComponentStateView>,
    pub virtual_time: VirtualTimeView,
}

pub struct RestoreRequestView {
    pub saved_state: SavedVmStateView,
    pub selected_vp_count: nat,
    pub downtime_ns: nat,
    pub has_time_adjustment: bool,
}

pub enum VmExecutionPhase {
    PreparingRestore,
    PreExecutionRestored,
    ReadyToRun,
    Running,
}

pub struct LoadedVmView {
    pub state: VmStateView,
    pub active_vp_count: nat,
    pub execution_phase: VmExecutionPhase,
}

pub open spec fn snapshot_state(state: VmStateView) -> SnapshotVmStateView {
    SnapshotVmStateView {
        memory: state.memory,
        partition_state: state.partition_state,
        vp_states: state.vp_states,
        component_inventory: state.component_inventory,
        active_component_state: state.active_component_state,
        pending_component_state: state.pending_component_state,
        virtual_time: state.virtual_time,
    }
}

pub open spec fn vm_time_after_downtime(
    snapshot_time_100ns: u64,
    downtime_ns: nat,
) -> u64 {
    ((snapshot_time_100ns as nat + downtime_ns / 100)
        % 0x1_0000_0000_0000_0000) as u64
}

pub open spec fn restore_vp_projection(
    initial_vps: Map<nat, VpStateView>,
    saved_vps: Map<nat, VpStateView>,
    selected_vp_count: nat,
) -> Map<nat, VpStateView> {
    Map::new(
        initial_vps.dom(),
        |vp_index: nat|
            if vp_index < selected_vp_count && saved_vps.dom().contains(vp_index) {
                saved_vps[vp_index]
            } else {
                initial_vps[vp_index]
            },
    )
}

pub open spec fn overlay_component_state(
    initial: Map<ComponentId, ComponentStateView>,
    restored: Map<ComponentId, ComponentStateView>,
) -> Map<ComponentId, ComponentStateView> {
    Map::new(
        initial.dom(),
        |component: ComponentId|
            if restored.dom().contains(component) {
                restored[component]
            } else {
                initial[component]
            },
    )
}

pub open spec fn restored_virtual_time(
    saved: VirtualTimeView,
    request: RestoreRequestView,
) -> VirtualTimeView {
    VirtualTimeView {
        vm_time_100ns: if request.has_time_adjustment {
            vm_time_after_downtime(saved.vm_time_100ns, request.downtime_ns)
        } else {
            saved.vm_time_100ns
        },
        elapsed_since_snapshot_ns: if request.has_time_adjustment {
            request.downtime_ns
        } else {
            0
        },
    }
}

pub open spec fn restore_snapshot_projection(
    initial: SnapshotVmStateView,
    request: RestoreRequestView,
    selected_vp_count: nat,
) -> SnapshotVmStateView {
    SnapshotVmStateView {
        memory: initial.memory,
        partition_state: request.saved_state.partition_state,
        vp_states: restore_vp_projection(
            initial.vp_states,
            request.saved_state.vp_states,
            selected_vp_count,
        ),
        component_inventory: request.saved_state.component_inventory,
        active_component_state: overlay_component_state(
            initial.active_component_state,
            request.saved_state.active_component_state,
        ),
        pending_component_state: overlay_component_state(
            initial.pending_component_state,
            request.saved_state.pending_component_state,
        ),
        virtual_time: restored_virtual_time(request.saved_state.virtual_time, request),
    }
}

pub open spec fn snapshot_restore_result(
    initial: VmStateView,
    selected_vp_count: nat,
    request: RestoreRequestView,
    restored: LoadedVmView,
) -> bool {
    snapshot_state(restored.state) == restore_snapshot_projection(
        snapshot_state(initial),
        request,
        selected_vp_count,
    )
    && restored.state.compatibility == initial.compatibility
    && restored.state.vp_capacity == initial.vp_capacity
    && restored.state.resources == initial.resources
    && restored.active_vp_count == selected_vp_count
    && restored.execution_phase == VmExecutionPhase::PreExecutionRestored
}

pub open spec fn snapshot_restore_success(
    initial: LoadedVmView,
    request: RestoreRequestView,
    restored: LoadedVmView,
) -> bool {
    snapshot_restore_result(
        initial.state,
        initial.active_vp_count,
        request,
        restored,
    )
}

impl RestoreRequestView {
    pub open spec fn valid_for_loaded_vm(self, initial: LoadedVmView) -> bool {
        initial.execution_phase == VmExecutionPhase::PreparingRestore
        && initial.active_vp_count <= initial.state.vp_capacity
        && self.selected_vp_count == initial.active_vp_count
        && initial.vp_identity_is_valid()
        && self.saved_state_is_compatible_with_state(initial.state)
    }

    pub open spec fn valid_for_initialized_vm(self, initial: InitializedVmView) -> bool {
        initial.boot_online_vps <= self.selected_vp_count
        && self.selected_vp_count <= initial.state.vp_capacity
        && initial.vp_identity_is_valid()
        && self.saved_state_is_compatible_with_state(initial.state)
    }

    pub open spec fn saved_state_is_compatible_with_state(
        self,
        initial: VmStateView,
    ) -> bool {
        self.saved_state.vp_states.dom().subset_of(initial.vp_states.dom())
        && self.saved_state.component_inventory == initial.component_inventory
        && self.saved_state.active_component_state.dom()
            .subset_of(self.saved_state.component_inventory)
        && self.saved_state.pending_component_state.dom()
            .subset_of(self.saved_state.component_inventory)
        && self.saved_state.active_component_state.dom()
            .subset_of(initial.active_component_state.dom())
        && self.saved_state.pending_component_state.dom()
            .subset_of(initial.pending_component_state.dom())
        && self.saved_state.virtual_time.elapsed_since_snapshot_ns == 0
    }
}

impl LoadedVmView {
    pub open spec fn vp_identity_is_valid(self) -> bool {
        forall |vp_index: nat|
            self.state.vp_states.dom().contains(vp_index)
                <==> vp_index < self.state.vp_capacity
    }
}

impl InitializedVmView {
    pub open spec fn vp_identity_is_valid(self) -> bool {
        forall |vp_index: nat|
            self.state.vp_states.dom().contains(vp_index)
                <==> vp_index < self.state.vp_capacity
    }
}

pub open spec fn snapshot_load_success(
    initial: InitializedVmView,
    request: RestoreRequestView,
    loaded: LoadedVmView,
) -> bool {
    snapshot_restore_result(
        initial.state,
        request.selected_vp_count,
        request,
        loaded,
    )
}

} // verus!
