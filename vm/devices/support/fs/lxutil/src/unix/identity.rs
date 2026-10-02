// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-thread filesystem identities for operations performed on behalf of another user.
//!
//! Linux checks filesystem access against a thread's filesystem UID and GID, supplementary
//! groups, and effective capabilities. All of these are per-thread credentials, so a file server
//! can act for a client on one thread without affecting any other thread in the process. The
//! credential syscalls are made directly: the libc wrappers for `setgroups` change every thread.

use super::util::check_lx_errno;
use caps::CapSet;
use caps::Capability;
use caps::CapsHashSet;
use std::marker::PhantomData;

/// Returns whether the calling thread holds the capabilities that [`FsIdentityScope::enter`]
/// requires.
pub fn has_fs_identity_capabilities() -> bool {
    caps::read(None, CapSet::Effective).is_ok_and(|effective| has_identity_capabilities(&effective))
}

fn has_identity_capabilities(effective: &CapsHashSet) -> bool {
    effective.contains(&Capability::CAP_SETUID) && effective.contains(&Capability::CAP_SETGID)
}

/// Makes the calling thread perform filesystem operations as another user until it is dropped.
///
/// Entering a scope requires `CAP_SETUID` and `CAP_SETGID` in the thread's effective capability
/// set. The scope clears the thread's supplementary groups, sets its filesystem UID and GID, and
/// then clears its effective capabilities. The kernel therefore checks every filesystem operation
/// in the scope exactly as it would for an unprivileged process with that UID and GID: new files
/// belong to that user, and operations that need a capability, such as changing a file's owner or
/// creating a device node, fail.
///
/// Every step is verified. If any step fails, the thread's original identity is restored and
/// `EPERM` is returned, so a caller never proceeds with the wrong identity. Dropping the scope
/// restores the original groups, filesystem IDs, and effective capabilities; if that cannot be
/// verified, the process panics rather than continuing with a corrupted thread identity.
///
/// The scope belongs to the thread that entered it and must not be nested. Threads created while
/// it is active inherit the switched identity.
#[must_use]
pub struct FsIdentityScope {
    fsuid: lx::uid_t,
    fsgid: lx::gid_t,
    groups: Vec<libc::gid_t>,
    effective: CapsHashSet,
    applied: Step,
    // Credentials are per-thread, so the scope must not move to another thread.
    _thread: PhantomData<*const ()>,
}

/// The identity changes that a scope has applied, in the order they are applied.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    None,
    Groups,
    Fsgid,
    Fsuid,
    Capabilities,
}

impl FsIdentityScope {
    /// Switches the calling thread's filesystem identity to `uid` and `gid`.
    ///
    /// Returns `EPERM` without changing the thread if the identity is invalid, the thread lacks
    /// `CAP_SETUID` or `CAP_SETGID`, or the kernel does not apply any part of the switch.
    pub fn enter(uid: lx::uid_t, gid: lx::gid_t) -> lx::Result<Self> {
        if uid == lx::UID_INVALID || gid == lx::GID_INVALID {
            return Err(lx::Error::EPERM);
        }
        let effective = caps::read(None, CapSet::Effective).map_err(|_| lx::Error::EPERM)?;
        if !has_identity_capabilities(&effective) {
            return Err(lx::Error::EPERM);
        }
        let groups = get_groups().map_err(|_| lx::Error::EPERM)?;

        // Dropping the scope on an early return undoes the steps that were applied.
        let mut scope = Self {
            fsuid: query_fsuid(),
            fsgid: query_fsgid(),
            groups,
            effective,
            applied: Step::None,
            _thread: PhantomData,
        };

        // The process's supplementary groups must not grant access to the new identity.
        set_groups(&[]).map_err(|_| lx::Error::EPERM)?;
        scope.applied = Step::Groups;

        set_fsgid(gid);
        scope.applied = Step::Fsgid;
        if query_fsgid() != gid {
            return Err(lx::Error::EPERM);
        }

        set_fsuid(uid);
        scope.applied = Step::Fsuid;
        if query_fsuid() != uid {
            return Err(lx::Error::EPERM);
        }

        caps::clear(None, CapSet::Effective).map_err(|_| lx::Error::EPERM)?;
        scope.applied = Step::Capabilities;
        if !caps::read(None, CapSet::Effective).is_ok_and(|effective| effective.is_empty()) {
            return Err(lx::Error::EPERM);
        }
        Ok(scope)
    }
}

impl Drop for FsIdentityScope {
    fn drop(&mut self) {
        if self.applied >= Step::Capabilities {
            // Restore the capabilities first; restoring the groups requires CAP_SETGID.
            restore_effective_capabilities(&self.effective);
        }
        if self.applied >= Step::Fsuid {
            set_fsuid(self.fsuid);
            assert_eq!(
                query_fsuid(),
                self.fsuid,
                "failed to restore the thread's filesystem UID"
            );
        }
        if self.applied >= Step::Fsgid {
            set_fsgid(self.fsgid);
            assert_eq!(
                query_fsgid(),
                self.fsgid,
                "failed to restore the thread's filesystem GID"
            );
        }
        if self.applied >= Step::Groups {
            set_groups(&self.groups).expect("failed to restore the thread's supplementary groups");
            assert_eq!(
                get_groups().ok().as_ref(),
                Some(&self.groups),
                "failed to restore the thread's supplementary groups"
            );
        }
        if self.applied >= Step::Fsuid {
            // Changing the filesystem UID to or from 0 also changes the effective capabilities,
            // so restore the original set exactly.
            restore_effective_capabilities(&self.effective);
        }
    }
}

fn restore_effective_capabilities(effective: &CapsHashSet) {
    caps::set(None, CapSet::Effective, effective)
        .expect("failed to restore the thread's effective capabilities");
    assert_eq!(
        caps::read(None, CapSet::Effective).ok().as_ref(),
        Some(effective),
        "failed to restore the thread's effective capabilities"
    );
}

fn get_groups() -> lx::Result<Vec<libc::gid_t>> {
    // SAFETY: A zero-length query only returns the number of supplementary groups.
    let count = check_lx_errno(unsafe {
        libc::syscall(
            libc::SYS_getgroups,
            0 as libc::c_long,
            std::ptr::null_mut::<libc::gid_t>(),
        )
    })?;
    let mut groups = vec![0; count as usize];
    // SAFETY: The buffer has room for `groups.len()` group IDs.
    let count = check_lx_errno(unsafe {
        libc::syscall(
            libc::SYS_getgroups,
            groups.len() as libc::c_long,
            groups.as_mut_ptr(),
        )
    })?;
    groups.truncate(count as usize);
    Ok(groups)
}

fn set_groups(groups: &[libc::gid_t]) -> lx::Result<()> {
    // SAFETY: The pointer and length describe `groups`, which outlives the call. The kernel does
    // not read the pointer of an empty list. The raw syscall changes only the calling thread.
    check_lx_errno(unsafe {
        libc::syscall(
            libc::SYS_setgroups,
            groups.len() as libc::c_long,
            groups.as_ptr(),
        )
    })?;
    Ok(())
}

/// Sets the calling thread's filesystem UID; `lx::UID_INVALID` only queries it.
///
/// Returns the previous value, whether or not the kernel applied the change.
fn set_fsuid(uid: lx::uid_t) -> lx::uid_t {
    // SAFETY: setfsuid takes an integer and changes only the calling thread's credentials.
    unsafe { libc::syscall(libc::SYS_setfsuid, uid as libc::c_long) as lx::uid_t }
}

/// Sets the calling thread's filesystem GID; `lx::GID_INVALID` only queries it.
///
/// Returns the previous value, whether or not the kernel applied the change.
fn set_fsgid(gid: lx::gid_t) -> lx::gid_t {
    // SAFETY: setfsgid takes an integer and changes only the calling thread's credentials.
    unsafe { libc::syscall(libc::SYS_setfsgid, gid as libc::c_long) as lx::gid_t }
}

fn query_fsuid() -> lx::uid_t {
    set_fsuid(lx::UID_INVALID)
}

fn query_fsgid() -> lx::gid_t {
    set_fsgid(lx::GID_INVALID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LxCreateOptions;
    use crate::LxVolume;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    /// An identity that no test host is expected to use.
    const UID: lx::uid_t = 4242;
    const GID: lx::gid_t = 4243;

    #[derive(Debug, PartialEq, Eq)]
    struct ThreadIdentity {
        fsuid: lx::uid_t,
        fsgid: lx::gid_t,
        groups: Vec<libc::gid_t>,
        effective: CapsHashSet,
    }

    fn thread_identity() -> ThreadIdentity {
        ThreadIdentity {
            fsuid: query_fsuid(),
            fsgid: query_fsgid(),
            groups: get_groups().unwrap(),
            effective: caps::read(None, CapSet::Effective).unwrap(),
        }
    }

    fn require_identity_capabilities() {
        assert!(
            has_fs_identity_capabilities(),
            "this test requires CAP_SETUID and CAP_SETGID; run it as root"
        );
    }

    /// Creates a directory that every user can write.
    fn shared_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        directory
    }

    #[test]
    fn missing_capabilities_fail_closed() {
        // Capabilities are per-thread, so drop them on a dedicated thread.
        std::thread::spawn(|| {
            caps::clear(None, CapSet::Effective).unwrap();
            let before = thread_identity();
            assert!(!has_fs_identity_capabilities());
            assert_eq!(
                FsIdentityScope::enter(UID, GID).err(),
                Some(lx::Error::EPERM)
            );
            assert_eq!(thread_identity(), before);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn invalid_identities_fail_closed() {
        let before = thread_identity();
        for (uid, gid) in [(lx::UID_INVALID, GID), (UID, lx::GID_INVALID)] {
            assert_eq!(
                FsIdentityScope::enter(uid, gid).err(),
                Some(lx::Error::EPERM)
            );
        }
        assert_eq!(thread_identity(), before);
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_switches_and_restores_the_thread() {
        require_identity_capabilities();
        let before = thread_identity();
        {
            let _scope = FsIdentityScope::enter(UID, GID).unwrap();
            assert_eq!(
                thread_identity(),
                ThreadIdentity {
                    fsuid: UID,
                    fsgid: GID,
                    groups: Vec::new(),
                    effective: CapsHashSet::new(),
                }
            );
        }
        assert_eq!(thread_identity(), before);
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_owns_new_files_and_cannot_escalate() {
        require_identity_capabilities();
        let directory = shared_directory();
        let volume = LxVolume::new(directory.path()).unwrap();
        {
            let _scope = FsIdentityScope::enter(UID, GID).unwrap();
            volume
                .open(
                    "file",
                    lx::O_CREAT | lx::O_EXCL | lx::O_RDWR,
                    Some(LxCreateOptions::new(0o644, 0, 0)),
                )
                .unwrap();
            volume
                .mkdir("directory", LxCreateOptions::new(0o755, 0, 0))
                .unwrap();
            // Without capabilities, the caller cannot give a file away or create a device.
            assert_eq!(
                volume.chown("file", Some(0), Some(0)).unwrap_err(),
                lx::Error::EPERM
            );
            assert_eq!(
                volume
                    .mknod(
                        "device",
                        LxCreateOptions::new(lx::S_IFCHR | 0o600, 0, 0),
                        0x103
                    )
                    .unwrap_err(),
                lx::Error::EPERM
            );
        }
        for name in ["file", "directory"] {
            let metadata = std::fs::symlink_metadata(directory.path().join(name)).unwrap();
            assert_eq!((metadata.uid(), metadata.gid()), (UID, GID), "{name}");
        }
        assert!(!directory.path().join("device").exists());
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_applies_only_to_the_calling_thread() {
        require_identity_capabilities();
        let before = thread_identity();
        // A thread created inside a scope would inherit its credentials, so start the observer
        // before entering the scope.
        let (entered_sender, entered) = std::sync::mpsc::channel();
        let observer = std::thread::spawn(move || {
            entered.recv().unwrap();
            thread_identity()
        });
        let _scope = FsIdentityScope::enter(UID, GID).unwrap();
        entered_sender.send(()).unwrap();
        assert_eq!(observer.join().unwrap(), before);
    }
}
