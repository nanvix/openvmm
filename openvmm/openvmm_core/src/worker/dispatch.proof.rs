// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Engineer-owned closed specifications and proof lemmas for `dispatch.rs`.
//
// Keep Human-owned restore semantics in `dispatch.spec.rs`. Add substantial
// proof functions here as frontend limitations are removed.

use super::InitializedVm;
use super::LoadedVm;
use super::restore_spec::RestoreRequestView;
use super::restore_spec::InitializedVmView;
use super::restore_spec::LoadedVmView;
use openvmm_defs::worker::SavedState;
use std::time::Duration;
use vstd::prelude::*;

verus! {

// The snapshot artifact and memory backing are prepared before load. This
// bridge exposes only the SavedState-owned state and the restore policy.
pub uninterp spec fn decoded_restore_request_view(
    saved_state: &Option<SavedState>,
    restore_time: &Option<(Duration, u64, Option<u64>)>,
    restore_vp_count: &Option<u32>,
) -> RestoreRequestView;

// TODO(uninterp): Replace with component Views for processor topology, memory,
// component identity, compatibility, and external resource identity.
pub uninterp spec fn initialized_vm_representation(vm: &InitializedVm) -> InitializedVmView;

impl View for InitializedVm {
    type V = InitializedVmView;

    closed spec fn view(&self) -> InitializedVmView {
        initialized_vm_representation(self)
    }
}

// TODO(uninterp): Replace with component Views for partition/VP state,
// components, memory, virtual time, deferred devices, and lifecycle state.
pub uninterp spec fn loaded_vm_representation(vm: &LoadedVm) -> LoadedVmView;

impl View for LoadedVm {
    type V = LoadedVmView;

    closed spec fn view(&self) -> LoadedVmView {
        loaded_vm_representation(self)
    }
}

pub closed spec fn pre_execution_representation(
    loaded: &LoadedVm,
    restored_from_snapshot: bool,
) -> bool {
    loaded.restored_from_snapshot == restored_from_snapshot
    && loaded.restore_start_guard.is_some() == restored_from_snapshot
    && !loaded.running
}

} // verus!
