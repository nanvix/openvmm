// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Caller identities for microVM shares with the caller owner policy.
//!
//! With the caller policy, the host performs each guest request as the UID and GID that the guest
//! kernel reports for the calling process, rather than as the VMM process. Requests from guest
//! UID 0 or GID 0 are squashed to the owner of the export root, so the guest can never create a
//! root-owned or setuid-root file on the host. If the host cannot switch to the caller identity,
//! the request fails with `EPERM`; it never falls back to the VMM identity.

use super::profile::MicroVmOwner;
use super::profile::MicroVmVirtioFsProfile;
use crate::VirtioFs;
use fuse::FuseOperation;
use fuse::Request;
use fuse::RequestScope;

/// Maps guest callers to host identities for a share with the caller owner policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CallerIdentity {
    squash_uid: lx::uid_t,
    squash_gid: lx::gid_t,
}

impl CallerIdentity {
    /// Builds the policy for an export root, whose owner receives guest UID 0 and GID 0.
    pub(crate) fn for_export_root(root: &lx::Stat) -> anyhow::Result<Self> {
        anyhow::ensure!(
            root.uid != 0 && root.gid != 0,
            "microVM virtio-fs caller identity requires an export root owned by a non-root user \
             and group, but the root is owned by {}:{}",
            root.uid,
            root.gid
        );
        Ok(Self {
            squash_uid: root.uid,
            squash_gid: root.gid,
        })
    }

    /// Returns the host UID and GID for a guest caller.
    pub(crate) fn host_identity(&self, uid: lx::uid_t, gid: lx::gid_t) -> (lx::uid_t, lx::gid_t) {
        (
            if uid == 0 { self.squash_uid } else { uid },
            if gid == 0 { self.squash_gid } else { gid },
        )
    }
}

/// Resolves the owner policy of a new share whose export root has the attributes `root`.
pub(crate) fn caller_identity(
    profile: &MicroVmVirtioFsProfile,
    root: &lx::Stat,
) -> anyhow::Result<Option<CallerIdentity>> {
    match profile.owner() {
        MicroVmOwner::Vmm => Ok(None),
        MicroVmOwner::Caller => {
            require_caller_identity_support()?;
            let identity = CallerIdentity::for_export_root(root)?;
            #[cfg(target_os = "linux")]
            if !lxutil::has_fs_identity_capabilities() {
                tracing::warn!(
                    "microVM virtio-fs caller identity requires CAP_SETUID and CAP_SETGID; every \
                     guest request on the share fails with EPERM"
                );
            }
            Ok(Some(identity))
        }
    }
}

/// Fails unless the host can switch identities for each request.
pub(crate) fn require_caller_identity_support() -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "microVM virtio-fs caller identity requires a Linux host"
    );
    Ok(())
}

/// Returns whether a request may act on host objects.
///
/// Releasing handles and tearing down the session only drop host descriptors, which involves no
/// access check, so they must succeed even when the caller identity is unavailable.
fn acts_on_host_objects(operation: &FuseOperation) -> bool {
    !matches!(
        operation,
        FuseOperation::Release { .. }
            | FuseOperation::ReleaseDir { .. }
            | FuseOperation::Destroy { .. }
            | FuseOperation::Interrupt { .. }
    )
}

/// Enters the host identity of the guest caller for one request, if the share requires it.
pub(crate) fn enter_request(fs: &VirtioFs, request: &Request) -> lx::Result<Option<RequestScope>> {
    let Some(identity) = fs.inner.caller_identity.as_ref() else {
        return Ok(None);
    };
    if !acts_on_host_objects(request.operation()) {
        return Ok(None);
    }
    let (uid, gid) = identity.host_identity(request.uid(), request.gid());
    enter_host_identity(uid, gid).map(Some)
}

#[cfg(target_os = "linux")]
fn enter_host_identity(uid: lx::uid_t, gid: lx::gid_t) -> lx::Result<RequestScope> {
    match lxutil::FsIdentityScope::enter(uid, gid) {
        Ok(scope) => Ok(Box::new(scope)),
        Err(error) => {
            tracelimit::warn_ratelimited!(
                uid,
                gid,
                error = &error as &dyn std::error::Error,
                "failed to enter the caller identity for a virtio-fs request"
            );
            Err(lx::Error::EPERM)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn enter_host_identity(_uid: lx::uid_t, _gid: lx::gid_t) -> lx::Result<RequestScope> {
    // A share with the caller policy cannot be created on this host.
    Err(lx::Error::EPERM)
}
