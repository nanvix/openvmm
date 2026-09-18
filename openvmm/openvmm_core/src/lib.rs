// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate implements the core VM worker for OpenVMM. This includes static
//! configuration of OpenVMM platform resources, such as the clock source and
//! memory management.
//!
//! Try not to add new functionality to this crate. Add it to other crates, and
//! reference new functionality via `Resource`s when you can to minimize build
//! time.

#![cfg_attr(not(verus_keep_ghost), forbid(unsafe_code))]
#![cfg_attr(verus_keep_ghost, feature(allocator_api, proc_macro_hygiene))]

mod emuplat;
pub mod hypervisor_backend;
mod partition;
#[allow(dead_code)]
mod verus_compat;
mod vmgs_non_volatile_store;
mod worker;

pub use worker::dispatch::VmWorker;
