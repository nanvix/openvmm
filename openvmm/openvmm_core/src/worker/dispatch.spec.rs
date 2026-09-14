// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Human-owned open specification vocabulary for `dispatch.rs` snapshot restore.
//
// This module contains no executable restore implementation. Its abstract
// Views describe the observable state at the `InitializedVm::load` boundary.

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
    pub saved_state: Option<SavedVmStateView>,
    pub selected_vp_count: nat,
    pub downtime_ns: nat,
    pub has_time_adjustment: bool,
}

pub enum VmExecutionPhase {
    PreExecutionRestored,
    ReadyToRun,
    Running,
}

pub struct LoadedVmView {
    pub state: VmStateView,
    pub active_vp_count: nat,
    pub execution_phase: VmExecutionPhase,
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
        |vp_index: nat| initial_vps.dom().contains(vp_index),
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
        |component: ComponentId| initial.dom().contains(component),
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

pub open spec fn restore_projection(
    initial: InitializedVmView,
    saved: SavedVmStateView,
    request: RestoreRequestView,
) -> LoadedVmView {
    LoadedVmView {
        state: VmStateView {
            memory: initial.state.memory,
            compatibility: initial.state.compatibility,
            vp_capacity: initial.state.vp_capacity,
            partition_state: saved.partition_state,
            vp_states: restore_vp_projection(
                initial.state.vp_states,
                saved.vp_states,
                request.selected_vp_count,
            ),
            component_inventory: initial.state.component_inventory,
            active_component_state: overlay_component_state(
                initial.state.active_component_state,
                saved.active_component_state,
            ),
            pending_component_state: overlay_component_state(
                initial.state.pending_component_state,
                saved.pending_component_state,
            ),
            virtual_time: restored_virtual_time(saved.virtual_time, request),
            resources: initial.state.resources,
        },
        active_vp_count: request.selected_vp_count,
        execution_phase: VmExecutionPhase::PreExecutionRestored,
    }
}

pub open spec fn snapshot_restore_success(
    initial: InitializedVmView,
    request: RestoreRequestView,
    loaded: LoadedVmView,
) -> bool {
    match request.saved_state {
        Some(saved) => loaded == restore_projection(initial, saved, request),
        None => false,
    }
}

impl RestoreRequestView {
    pub open spec fn valid_for(self, initial: InitializedVmView) -> bool {
        match self.saved_state {
            Some(saved) => {
                self.saved_state_is_compatible_with(initial, saved)
                && self.boot_vp_count_is_valid(initial)
            }
            None => false,
        }
    }

    pub open spec fn saved_state_is_compatible_with(
        self,
        initial: InitializedVmView,
        saved: SavedVmStateView,
    ) -> bool {
        initial.vp_identity_is_valid()
        && saved.vp_states.dom().subset_of(initial.state.vp_states.dom())
        && saved.component_inventory == initial.state.component_inventory
        && saved.active_component_state.dom().subset_of(saved.component_inventory)
        && saved.pending_component_state.dom().subset_of(saved.component_inventory)
        && saved.active_component_state.dom()
            .subset_of(initial.state.active_component_state.dom())
        && saved.pending_component_state.dom()
            .subset_of(initial.state.pending_component_state.dom())
        && saved.virtual_time.elapsed_since_snapshot_ns == 0
    }

    pub open spec fn boot_vp_count_is_valid(self, initial: InitializedVmView) -> bool {
        initial.boot_online_vps <= self.selected_vp_count
        && self.selected_vp_count <= initial.state.vp_capacity
    }
}

impl InitializedVmView {
    pub open spec fn vp_identity_is_valid(self) -> bool {
        forall |vp_index: nat|
            self.state.vp_states.dom().contains(vp_index)
                <==> vp_index < self.state.vp_capacity
    }
}

} // verus!
