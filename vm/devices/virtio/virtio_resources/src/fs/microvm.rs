// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM profile of the virtio-fs resource.
//!
//! [`VirtioFsProfile`] selects the standard virtio-fs device or the microVM
//! device, which serves an attached host folder, an aggregate of host folders
//! backed by [`super::VirtioFsBackend::Aggregate`], or a dormant slot backed
//! by [`super::VirtioFsBackend::Dormant`].

use mesh::MeshPayload;

/// Read-only, little-endian microVM filesystem layout appended to the
/// standard virtio-fs config. Child indices refer to attachment/receipt order;
/// child names and host identities come from the authenticated activation,
/// not from guest-supplied paths or a boot command-line token.
pub mod layout {
    /// Byte offset in device config, after the tag and request queue count.
    pub const CONFIG_OFFSET: u16 = 40;
    /// `MVFS` in little-endian byte order.
    pub const MAGIC: u32 = u32::from_le_bytes(*b"MVFS");
    /// Layout schema version, independent of the platform-template envelope.
    pub const VERSION: u32 = 1;
    /// Eight u32 fields: magic, version, header size, state, access, owner,
    /// child count, and child record size.
    pub const HEADER_SIZE: u32 = 32;
    /// Each child is a u16 index followed by u8 kind and u8 access.
    pub const CHILD_SIZE: u32 = 4;
    /// No backend; no access mode or children.
    pub const DORMANT: u32 = 0;
    /// One directory backend; no child records.
    pub const SINGLE: u32 = 1;
    /// Synthetic root with the following ordered child records.
    pub const AGGREGATE: u32 = 2;
    /// Directory child.
    pub const DIRECTORY: u8 = 1;
    /// Regular-file child.
    pub const FILE: u8 = 2;
    /// Host-enforced read-only access.
    pub const READ_ONLY: u8 = 1;
    /// Host-enforced read-write access, subject to subtree policy.
    pub const READ_WRITE: u8 = 2;
    /// Host operations run as the VMM.
    pub const OWNER_VMM: u32 = 0;
    /// Host operations run as the guest caller with root squashed.
    pub const OWNER_CALLER: u32 = 1;
}

#[derive(MeshPayload)]
pub enum VirtioFsProfile {
    Standard,
    Microvm {
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        /// Canonical share-relative paths hidden from the guest.
        denied_paths: Vec<String>,
        /// Canonical share-relative paths inside `denied_paths` that the guest
        /// can reach again.
        allowed_paths: Vec<String>,
        /// Canonical share-relative paths that are the only parts of a
        /// read-write share that the guest can modify; none makes the whole
        /// share writable.
        writable_paths: Vec<String>,
        /// Perform each guest request as the host UID and GID of its caller,
        /// with root squashed to the owner of the export root, instead of as
        /// the VMM. Linux only.
        caller_identity: bool,
    },
    /// An aggregate whose synthetic, read-only root lists one directory or
    /// regular file per child. The children of the `Aggregate` backend must
    /// have the same names, in the same order.
    MicrovmAggregate {
        stable_id: String,
        children: Vec<MicrovmAggregateChild>,
        /// Perform each guest request as its caller, like
        /// [`VirtioFsProfile::Microvm`]'s `caller_identity`. Every child root
        /// must then have the same owner.
        caller_identity: bool,
    },
    MicrovmDormant {
        stable_id: String,
    },
}

/// The identity and access policy of one child of a
/// [`VirtioFsProfile::MicrovmAggregate`], with the meaning of the
/// corresponding fields of [`VirtioFsProfile::Microvm`].
#[derive(MeshPayload)]
pub struct MicrovmAggregateChild {
    /// Name of the child under the synthetic root.
    pub name: String,
    /// The kind of host object that the child's root path names.
    pub kind: MicrovmAggregateChildKind,
    pub root_identity: Vec<u8>,
    pub read_only: bool,
    pub denied_paths: Vec<String>,
    pub allowed_paths: Vec<String>,
    pub writable_paths: Vec<String>,
}

/// The kind of host object that a [`MicrovmAggregateChild`] exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, MeshPayload)]
pub enum MicrovmAggregateChildKind {
    /// A host directory, with its own denied, allowed, and writable paths.
    Directory,
    /// A regular host file, without its directory. Its access mode applies to
    /// the whole file, so it takes no denied, allowed, or writable paths.
    File,
}
