// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// TODO: move to a separate crate.

pub mod igvm;
pub mod linux;
pub mod pcat;
#[cfg(guest_arch = "x86_64")]
pub mod pvh;
pub mod uefi;
