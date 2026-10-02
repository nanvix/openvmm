// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for microVM shares with the caller owner policy.

use super::identity::CallerIdentity;
use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::Request;
use fuse::protocol::FUSE_MKDIR;
use fuse::protocol::fuse_in_header;
use fuse::protocol::fuse_mkdir_in;
use std::path::Path;
use tempfile::tempdir;
use test_with_tracing::test;
use zerocopy::IntoBytes;

fn profile(root_path: &Path, caller_identity: bool) -> MicroVmVirtioFsProfile {
    MicroVmVirtioFsProfile::from_attachment(
        MICROVM_ATTACHMENT_ID.to_owned(),
        microvm_root_identity(root_path).unwrap(),
        false,
        Vec::new(),
        caller_identity,
    )
    .unwrap()
}

/// Builds a FUSE request as the guest kernel sends it on behalf of `caller`.
fn message(opcode: u32, node_id: u64, caller: (u32, u32), parts: &[&[u8]]) -> Vec<u8> {
    let length = size_of::<fuse_in_header>() + parts.iter().map(|part| part.len()).sum::<usize>();
    let header = fuse_in_header {
        len: length as u32,
        opcode,
        unique: 1,
        nodeid: node_id,
        uid: caller.0,
        gid: caller.1,
        pid: 1,
        padding: 0,
    };
    let mut data = header.as_bytes().to_vec();
    for part in parts {
        data.extend_from_slice(part);
    }
    data
}

fn c_string(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

fn mkdir(node_id: u64, name: &str, caller: (u32, u32)) -> Vec<u8> {
    let arg = fuse_mkdir_in {
        mode: 0o755,
        umask: 0,
    };
    message(
        FUSE_MKDIR,
        node_id,
        caller,
        &[arg.as_bytes(), &c_string(name)],
    )
}

/// Returns the attributes of a directory with the given owner.
fn root_stat(uid: u32, gid: u32) -> lx::Stat {
    let directory = tempdir().unwrap();
    let mut stat = lxutil::LxVolume::new(directory.path())
        .unwrap()
        .lstat("")
        .unwrap();
    stat.uid = uid;
    stat.gid = gid;
    stat
}

#[test]
fn guest_root_is_squashed_to_the_export_owner() {
    let identity = CallerIdentity::for_export_root(&root_stat(1000, 1001)).unwrap();
    assert_eq!(identity.host_identity(0, 0), (1000, 1001));
    // UID 0 and GID 0 are squashed independently.
    assert_eq!(identity.host_identity(0, 7), (1000, 7));
    assert_eq!(identity.host_identity(7, 0), (7, 1001));
    assert_eq!(identity.host_identity(4242, 4243), (4242, 4243));
}

#[test]
fn caller_identity_requires_a_non_root_export_owner() {
    for (uid, gid) in [(0, 1001), (1000, 0), (0, 0)] {
        let error = CallerIdentity::for_export_root(&root_stat(uid, gid)).unwrap_err();
        assert!(
            error.to_string().contains("non-root user and group"),
            "{error:#}"
        );
    }
}

#[test]
fn vmm_owner_requests_use_the_vmm_identity() {
    let directory = tempdir().unwrap();
    let fs = VirtioFs::new_microvm(directory.path(), profile(directory.path(), false)).unwrap();
    let request = Request::new(mkdir(1, "directory", (4242, 4242)).as_slice()).unwrap();
    assert!(fs.enter_request(&request).unwrap().is_none());
}

#[cfg(windows)]
#[test]
fn caller_identity_requires_a_linux_host() {
    let directory = tempdir().unwrap();
    let error = VirtioFs::new_microvm(directory.path(), profile(directory.path(), true))
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("requires a Linux host"),
        "{error:#}"
    );
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use caps::CapSet;
    use fuse::ReplySender;
    use fuse::Session;
    use fuse::protocol::FATTR_GID;
    use fuse::protocol::FATTR_MODE;
    use fuse::protocol::FATTR_UID;
    use fuse::protocol::FUSE_CREATE;
    use fuse::protocol::FUSE_DESTROY;
    use fuse::protocol::FUSE_FORGET;
    use fuse::protocol::FUSE_GETATTR;
    use fuse::protocol::FUSE_GETXATTR;
    use fuse::protocol::FUSE_INIT;
    use fuse::protocol::FUSE_LOOKUP;
    use fuse::protocol::FUSE_MKNOD;
    use fuse::protocol::FUSE_OPEN;
    use fuse::protocol::FUSE_OPENDIR;
    use fuse::protocol::FUSE_RELEASE;
    use fuse::protocol::FUSE_SETATTR;
    use fuse::protocol::FUSE_SETXATTR;
    use fuse::protocol::FUSE_STATFS;
    use fuse::protocol::FUSE_SYMLINK;
    use fuse::protocol::fuse_create_in;
    use fuse::protocol::fuse_entry_out;
    use fuse::protocol::fuse_forget_in;
    use fuse::protocol::fuse_getattr_in;
    use fuse::protocol::fuse_getxattr_in;
    use fuse::protocol::fuse_init_in;
    use fuse::protocol::fuse_mknod_in;
    use fuse::protocol::fuse_open_in;
    use fuse::protocol::fuse_out_header;
    use fuse::protocol::fuse_release_in;
    use fuse::protocol::fuse_setattr_in;
    use fuse::protocol::fuse_setxattr_in;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use test_with_tracing::test;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;

    const ROOT: (u32, u32) = (0, 0);
    const USER: (u32, u32) = (1001, 1001);
    const OTHER_USER: (u32, u32) = (1002, 1002);
    /// Owns the export root, so guest UID 0 and GID 0 map to it.
    const EXPORT_OWNER: (u32, u32) = (4242, 4242);

    /// The reply that the session sent for one request.
    #[derive(Default)]
    struct Reply(Vec<u8>);

    impl ReplySender for Reply {
        fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
            self.0 = bufs.iter().flat_map(|buf| buf.iter()).copied().collect();
            Ok(())
        }
    }

    impl Reply {
        /// Returns the positive error number of the reply, or 0 on success.
        fn error(&self) -> i32 {
            -fuse_out_header::read_from_prefix(&self.0).unwrap().0.error
        }

        fn node_id(&self) -> u64 {
            assert_eq!(self.error(), 0, "request failed");
            fuse_entry_out::read_from_prefix(&self.0[size_of::<fuse_out_header>()..])
                .unwrap()
                .0
                .nodeid
        }
    }

    fn lookup(node_id: u64, name: &str, caller: (u32, u32)) -> Vec<u8> {
        message(FUSE_LOOKUP, node_id, caller, &[&c_string(name)])
    }

    fn create(node_id: u64, name: &str, caller: (u32, u32)) -> Vec<u8> {
        let arg = fuse_create_in {
            flags: lx::O_RDWR as u32,
            mode: lx::S_IFREG | 0o644,
            umask: 0,
            padding: 0,
        };
        message(
            FUSE_CREATE,
            node_id,
            caller,
            &[arg.as_bytes(), &c_string(name)],
        )
    }

    fn mknod(node_id: u64, name: &str, mode: u32, caller: (u32, u32)) -> Vec<u8> {
        let arg = fuse_mknod_in {
            mode,
            rdev: 0x103,
            umask: 0,
            padding: 0,
        };
        message(
            FUSE_MKNOD,
            node_id,
            caller,
            &[arg.as_bytes(), &c_string(name)],
        )
    }

    fn symlink(node_id: u64, name: &str, target: &str, caller: (u32, u32)) -> Vec<u8> {
        message(
            FUSE_SYMLINK,
            node_id,
            caller,
            &[&c_string(name), &c_string(target)],
        )
    }

    fn setattr(
        node_id: u64,
        caller: (u32, u32),
        update: impl FnOnce(&mut fuse_setattr_in),
    ) -> Vec<u8> {
        let mut arg = fuse_setattr_in::new_zeroed();
        update(&mut arg);
        message(FUSE_SETATTR, node_id, caller, &[arg.as_bytes()])
    }

    fn open(node_id: u64, flags: i32, caller: (u32, u32)) -> Vec<u8> {
        let arg = fuse_open_in {
            flags: flags as u32,
            unused: 0,
        };
        message(FUSE_OPEN, node_id, caller, &[arg.as_bytes()])
    }

    fn setxattr(node_id: u64, name: &str, value: &[u8], caller: (u32, u32)) -> Vec<u8> {
        let arg = fuse_setxattr_in {
            size: value.len() as u32,
            flags: 0,
        };
        message(
            FUSE_SETXATTR,
            node_id,
            caller,
            &[arg.as_bytes(), &c_string(name), value],
        )
    }

    /// One request of every kind that reaches host objects.
    fn host_requests(caller: (u32, u32)) -> Vec<(&'static str, Vec<u8>)> {
        let getxattr = fuse_getxattr_in {
            size: 64,
            padding: 0,
        };
        let opendir = fuse_open_in {
            flags: (lx::O_RDONLY | lx::O_DIRECTORY) as u32,
            unused: 0,
        };
        vec![
            ("lookup", lookup(1, "host-file", caller)),
            (
                "getattr",
                message(
                    FUSE_GETATTR,
                    1,
                    caller,
                    &[fuse_getattr_in::new_zeroed().as_bytes()],
                ),
            ),
            (
                "setattr",
                setattr(1, caller, |arg| {
                    arg.valid = FATTR_MODE;
                    arg.mode = 0o700;
                }),
            ),
            ("mkdir", mkdir(1, "guest-directory", caller)),
            ("create", create(1, "guest-file", caller)),
            ("mknod", mknod(1, "guest-fifo", lx::S_IFIFO | 0o644, caller)),
            ("symlink", symlink(1, "guest-link", "host-file", caller)),
            (
                "opendir",
                message(FUSE_OPENDIR, 1, caller, &[opendir.as_bytes()]),
            ),
            ("statfs", message(FUSE_STATFS, 1, caller, &[])),
            (
                "getxattr",
                message(
                    FUSE_GETXATTR,
                    1,
                    caller,
                    &[getxattr.as_bytes(), &c_string("user.nvx")],
                ),
            ),
            ("setxattr", setxattr(1, "user.nvx", b"value", caller)),
        ]
    }

    /// A microVM share with the caller owner policy and an initialized FUSE session.
    struct Share {
        _directory: tempfile::TempDir,
        root: PathBuf,
        session: Session,
    }

    impl Share {
        /// Creates an export root that every user can write and that belongs to
        /// `EXPORT_OWNER` when the test can change its owner.
        fn new(prepare: impl FnOnce(&Path)) -> Self {
            let directory = tempdir().unwrap();
            let root = directory.path().join("share");
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
            let metadata = std::fs::metadata(&root).unwrap();
            if metadata.uid() == 0 || metadata.gid() == 0 {
                std::os::unix::fs::chown(&root, Some(EXPORT_OWNER.0), Some(EXPORT_OWNER.1))
                    .unwrap();
            }
            prepare(&root);
            let fs = VirtioFs::new_microvm(&root, profile(&root, true)).unwrap();
            let session = Session::new(fs);
            let init = fuse_init_in {
                major: 7,
                minor: 31,
                max_readahead: 0x20000,
                flags: 0,
                flags2: 0,
                unused: [0; 11],
            };
            let share = Self {
                _directory: directory,
                root,
                session,
            };
            assert_eq!(
                share
                    .dispatch(message(FUSE_INIT, 0, ROOT, &[init.as_bytes()]))
                    .error(),
                0
            );
            share
        }

        fn dispatch(&self, request: Vec<u8>) -> Reply {
            let mut reply = Reply::default();
            self.session
                .dispatch(Request::new(request.as_slice()).unwrap(), &mut reply, None);
            reply
        }

        fn owner(&self, path: &str) -> (u32, u32) {
            let metadata = std::fs::symlink_metadata(self.root.join(path)).unwrap();
            (metadata.uid(), metadata.gid())
        }

        fn names(&self) -> Vec<String> {
            let mut names = std::fs::read_dir(&self.root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    /// Returns the credentials of the calling thread as the kernel reports them.
    fn thread_credentials() -> Vec<String> {
        std::fs::read_to_string("/proc/thread-self/status")
            .unwrap()
            .lines()
            .filter(|line| {
                ["Uid:", "Gid:", "Groups:", "CapEff:"]
                    .iter()
                    .any(|prefix| line.starts_with(prefix))
            })
            .map(str::to_owned)
            .collect()
    }

    fn require_identity_capabilities() {
        assert!(
            lxutil::has_fs_identity_capabilities(),
            "this test requires CAP_SETUID and CAP_SETGID; run it as root"
        );
    }

    #[test]
    fn caller_identity_rejects_a_root_owned_export() {
        let root = std::fs::metadata("/").unwrap();
        if root.uid() != 0 || root.gid() != 0 {
            return;
        }
        let error = VirtioFs::new_microvm("/", profile(Path::new("/"), true))
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("non-root user and group"),
            "{error:#}"
        );
    }

    #[test]
    fn missing_capabilities_fail_host_requests_closed() {
        let share = Share::new(|root| {
            std::fs::write(root.join("host-file"), b"host").unwrap();
        });
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    // Capabilities are per-thread; this thread can no longer switch identities.
                    caps::clear(None, CapSet::Effective).unwrap();
                    for caller in [ROOT, USER] {
                        for (operation, request) in host_requests(caller) {
                            assert_eq!(
                                share.dispatch(request).error(),
                                lx::Error::EPERM.value(),
                                "{operation} as {caller:?}"
                            );
                        }
                    }
                    // Releasing handles and tearing down the session still succeed, and
                    // forget has no reply.
                    let release = fuse_release_in {
                        fh: 12345,
                        ..fuse_release_in::new_zeroed()
                    };
                    assert_eq!(
                        share
                            .dispatch(message(FUSE_RELEASE, 1, USER, &[release.as_bytes()]))
                            .error(),
                        0
                    );
                    let forget = fuse_forget_in { nlookup: 1 };
                    assert!(
                        share
                            .dispatch(message(FUSE_FORGET, 1, USER, &[forget.as_bytes()]))
                            .0
                            .is_empty()
                    );
                    assert_eq!(
                        share.dispatch(message(FUSE_DESTROY, 0, USER, &[])).error(),
                        0
                    );
                })
                .join()
                .unwrap();
        });
        // No request reached the host, so nothing ran as the VMM identity.
        assert_eq!(share.names(), ["host-file"]);
        assert_eq!(
            std::fs::read(share.root.join("host-file")).unwrap(),
            b"host"
        );
        let mode = std::fs::metadata(&share.root).unwrap().mode();
        assert_eq!(mode & 0o7777, 0o777);
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_maps_guest_requests_to_host_owners() {
        require_identity_capabilities();
        let share = Share::new(|_| {});
        assert_eq!(share.owner(""), EXPORT_OWNER);

        // Guest root is squashed to the owner of the export root.
        share.dispatch(mkdir(1, "root-directory", ROOT)).node_id();
        share.dispatch(create(1, "root-file", ROOT)).node_id();
        // A non-root caller owns what it creates, including inside its own directories.
        let directory = share.dispatch(mkdir(1, "user-directory", USER)).node_id();
        share.dispatch(create(directory, "nested", USER)).node_id();
        share
            .dispatch(symlink(1, "user-link", "user-directory", USER))
            .node_id();
        share
            .dispatch(mknod(1, "user-fifo", lx::S_IFIFO | 0o644, USER))
            .node_id();
        // UID 0 and GID 0 are squashed independently.
        share.dispatch(create(1, "root-uid", (0, USER.1))).node_id();
        share.dispatch(create(1, "root-gid", (USER.0, 0))).node_id();

        for (path, owner) in [
            ("root-directory", EXPORT_OWNER),
            ("root-file", EXPORT_OWNER),
            ("user-directory", USER),
            ("user-directory/nested", USER),
            ("user-link", USER),
            ("user-fifo", USER),
            ("root-uid", (EXPORT_OWNER.0, USER.1)),
            ("root-gid", (USER.0, EXPORT_OWNER.1)),
        ] {
            assert_eq!(share.owner(path), owner, "{path}");
        }
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_denies_privileged_host_operations() {
        require_identity_capabilities();
        let share = Share::new(|root| {
            // The test runs as root, so this file belongs to root.
            let secret = root.join("root-only");
            std::fs::write(&secret, b"secret").unwrap();
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        });
        let file = share.dispatch(create(1, "squashed-file", ROOT)).node_id();

        // Neither the export owner nor squashed guest root can give a file to root. The
        // group change also fails because the VMM's supplementary groups are dropped.
        let chown = setattr(file, ROOT, |arg| {
            arg.valid = FATTR_UID;
            arg.uid = 0;
        });
        assert_eq!(share.dispatch(chown).error(), lx::Error::EPERM.value());
        let chgrp = setattr(file, ROOT, |arg| {
            arg.valid = FATTR_GID;
            arg.gid = 0;
        });
        assert_eq!(share.dispatch(chgrp).error(), lx::Error::EPERM.value());

        // The VMM's capabilities do not apply: no device nodes, file capabilities, or
        // trusted attributes.
        let device = mknod(1, "device", lx::S_IFCHR | 0o600, ROOT);
        assert_eq!(share.dispatch(device).error(), lx::Error::EPERM.value());
        // A valid VFS_CAP_REVISION_2 value that grants CAP_SETUID.
        let mut capability = Vec::new();
        for word in [0x0200_0001u32, 1 << 7, 0, 0, 0] {
            capability.extend_from_slice(&word.to_le_bytes());
        }
        let file_capability = setxattr(file, "security.capability", &capability, ROOT);
        assert_eq!(
            share.dispatch(file_capability).error(),
            lx::Error::EPERM.value()
        );
        let trusted = setxattr(file, "trusted.nvx", b"value", ROOT);
        assert_eq!(share.dispatch(trusted).error(), lx::Error::EPERM.value());

        // Host permissions apply to the caller identity.
        let secret = share.dispatch(lookup(1, "root-only", ROOT)).node_id();
        assert_eq!(
            share.dispatch(open(secret, lx::O_RDONLY, ROOT)).error(),
            lx::Error::EACCES.value()
        );
        let user_file = share.dispatch(create(1, "user-file", USER)).node_id();
        assert_eq!(
            share
                .dispatch(open(user_file, lx::O_WRONLY, OTHER_USER))
                .error(),
            lx::Error::EACCES.value()
        );

        // An identity that the host cannot represent fails closed.
        assert_eq!(
            share
                .dispatch(create(1, "invalid", (lx::UID_INVALID, USER.1)))
                .error(),
            lx::Error::EPERM.value()
        );

        assert_eq!(share.owner("squashed-file"), EXPORT_OWNER);
        assert_eq!(share.names(), ["root-only", "squashed-file", "user-file"]);
    }

    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID (run as root)"]
    fn caller_identity_restores_the_dispatching_thread() {
        require_identity_capabilities();
        let share = Share::new(|_| {});
        let before = thread_credentials();
        for (operation, request) in host_requests(USER) {
            share.dispatch(request);
            assert_eq!(thread_credentials(), before, "{operation}");
        }
        share.dispatch(create(1, "invalid", (lx::UID_INVALID, USER.1)));
        assert_eq!(thread_credentials(), before);
    }
}
