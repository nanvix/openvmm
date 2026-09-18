// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(verus_keep_ghost, allow(unsafe_code))]

use vstd::prelude::*;

verus! {

#[verifier::external_type_specification]
#[verifier::external_body]
pub struct ExAnyhowError(anyhow::Error);

pub assume_specification<M: core::fmt::Display + core::fmt::Debug + Send + Sync + 'static>[
    anyhow::Error::msg::<M>
](message: M) -> (error: anyhow::Error);

pub assume_specification<'a>[
    anyhow::__private::format_err
](args: core::fmt::Arguments<'a>) -> (error: anyhow::Error);

pub assume_specification[
    anyhow::__private::must_use
](error: anyhow::Error) -> (result: anyhow::Error);

#[cfg(verus_keep_ghost)]
pub assume_specification<T: ?Sized, A: core::alloc::Allocator + Clone>[
    <std::sync::Arc<T, A> as Clone>::clone
](value: &std::sync::Arc<T, A>) -> (result: std::sync::Arc<T, A>);

} // verus!
