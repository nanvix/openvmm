// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Versioned, host-authorized opened-object grants and admission receipts.
//!
//! These identities name objects, not immutable contents. The storage owner
//! must separately prevent in-place writes to immutable media.

use mesh::MeshPayload;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::io;

pub const ATTACHMENT_HANDOFF_VERSION: u32 = 1;
pub const MAX_ATTACHMENT_GRANTS: usize = 260;

#[derive(Clone, Debug, PartialEq, Eq, MeshPayload, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AttachmentIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u64, file_id: [u8; 16] },
}

impl AttachmentIdentity {
    /// Queries the actual opened handle, never its pathname.
    pub fn query(file: &File) -> Result<Self, AttachmentError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = file.metadata()?;
            if metadata.ino() == 0 {
                return Err(AttachmentError::Invalid(
                    "opened object has no stable inode",
                ));
            }
            Ok(Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            let identity = pal::windows::fs::relative::object_identity(file)?;
            if identity.file_id == [0; 16] {
                return Err(AttachmentError::Invalid(
                    "opened object has no stable file ID",
                ));
            }
            Ok(Self::Windows {
                volume: identity.volume_serial_number,
                file_id: identity.file_id,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttachmentKind {
    Directory,
    File,
    BlockMedia,
}

#[derive(Clone, Debug, PartialEq, Eq, MeshPayload, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AttachmentBinding {
    ObjectPinned { identity: AttachmentIdentity },
    LivePath,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, MeshPayload, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentGeometry {
    pub logical_sector_size: u32,
    pub physical_sector_size: u32,
}

impl AttachmentGeometry {
    pub fn validate(self, length: u64) -> Result<(), AttachmentError> {
        if self.logical_sector_size < 512
            || !self.logical_sector_size.is_power_of_two()
            || self.physical_sector_size < self.logical_sector_size
            || !self.physical_sector_size.is_power_of_two()
            || length == 0
            || !length.is_multiple_of(u64::from(self.logical_sector_size))
        {
            return Err(AttachmentError::Invalid(
                "invalid media length or sector geometry",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentGrant {
    pub role: String,
    pub kind: AttachmentKind,
    pub read_only: bool,
    pub binding: AttachmentBinding,
    pub length: Option<u64>,
    pub geometry: Option<AttachmentGeometry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedRoot {
    pub path: String,
    pub identity: AttachmentIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentGrantSet {
    pub version: u32,
    /// Also used as the capability-authenticated control-console instance ID.
    pub activation_id: [u8; 16],
    /// Mandatory caller-owned roots that filesystem exports must not overlap.
    pub protected_roots: Vec<ProtectedRoot>,
    pub attachments: Vec<AttachmentGrant>,
}

impl AttachmentGrantSet {
    pub fn validate(&self) -> Result<(), AttachmentError> {
        if self.version != ATTACHMENT_HANDOFF_VERSION
            || self.activation_id == [0; 16]
            || self.attachments.len() > MAX_ATTACHMENT_GRANTS
            || self.protected_roots.is_empty()
            || self.protected_roots.len() > 128
        {
            return Err(AttachmentError::Invalid(
                "unsupported or unbounded attachment grant set",
            ));
        }
        for (index, grant) in self.attachments.iter().enumerate() {
            if grant.role.is_empty()
                || grant.role.len() > 128
                || !grant.role.is_ascii()
                || self.attachments[..index]
                    .iter()
                    .any(|other| other.role == grant.role)
            {
                return Err(AttachmentError::Invalid(
                    "invalid or duplicate attachment role",
                ));
            }
            match grant.kind {
                AttachmentKind::BlockMedia => {
                    let length = grant
                        .length
                        .ok_or(AttachmentError::Invalid("media length is required"))?;
                    grant
                        .geometry
                        .ok_or(AttachmentError::Invalid("media geometry is required"))?
                        .validate(length)?;
                    if !matches!(grant.binding, AttachmentBinding::ObjectPinned { .. }) {
                        return Err(AttachmentError::Invalid(
                            "verified media must be object-pinned",
                        ));
                    }
                }
                AttachmentKind::Directory if grant.length.is_some() || grant.geometry.is_some() => {
                    return Err(AttachmentError::Invalid(
                        "directory grants take no size or geometry",
                    ));
                }
                AttachmentKind::File if grant.geometry.is_some() => {
                    return Err(AttachmentError::Invalid(
                        "file grants take no sector geometry",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, MeshPayload, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentReceipt {
    pub role: String,
    pub kind: AttachmentKind,
    pub read_only: bool,
    pub binding: AttachmentBinding,
    pub identity: AttachmentIdentity,
    pub length: Option<u64>,
    pub geometry: Option<AttachmentGeometry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentReceiptSet {
    pub version: u32,
    pub activation_id: [u8; 16],
    pub attachments: Vec<AttachmentReceipt>,
}

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("invalid attachment contract: {0}")]
    Invalid(&'static str),
    #[error("attachment '{role}' has a mismatched {field}")]
    Mismatch { role: String, field: &'static str },
    #[error("failed to inspect opened attachment")]
    Io(#[from] io::Error),
}

impl AttachmentGrant {
    /// Admits exactly the object the VMM will use, after opening it with the
    /// requested access. `geometry` is the disk backend's effective geometry.
    pub fn verify_opened(
        &self,
        file: &File,
        role: &str,
        kind: AttachmentKind,
        read_only: bool,
        geometry: Option<AttachmentGeometry>,
    ) -> Result<AttachmentReceipt, AttachmentError> {
        let mismatch = |field| AttachmentError::Mismatch {
            role: role.to_owned(),
            field,
        };
        if self.role != role
            || self.kind != kind
            || self.read_only != read_only
            || self.geometry != geometry
        {
            return Err(mismatch("role, kind, mode, or geometry"));
        }
        let metadata = file.metadata()?;
        if kind != AttachmentKind::Directory {
            #[cfg(unix)]
            let access = pal::unix::fs::file_data_access(file)?;
            #[cfg(windows)]
            let access = pal::windows::fs::relative::file_data_access(file)?;
            if access != (true, !read_only) {
                return Err(mismatch("opened handle access"));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(mismatch("reparse-point kind"));
            }
        }
        if match kind {
            AttachmentKind::Directory => !metadata.is_dir(),
            AttachmentKind::File | AttachmentKind::BlockMedia => !metadata.is_file(),
        } {
            return Err(mismatch("opened object kind"));
        }
        let identity = AttachmentIdentity::query(file)?;
        if let AttachmentBinding::ObjectPinned { identity: expected } = &self.binding
            && *expected != identity
        {
            return Err(mismatch("opened identity"));
        }
        let length = (kind != AttachmentKind::Directory).then_some(metadata.len());
        if self.length.is_some() && self.length != length {
            return Err(mismatch("opened length"));
        }
        if let Some(geometry) = geometry {
            geometry.validate(metadata.len())?;
        }
        if kind == AttachmentKind::BlockMedia
            && (self.length.is_none()
                || geometry.is_none()
                || !matches!(self.binding, AttachmentBinding::ObjectPinned { .. }))
            || kind == AttachmentKind::Directory && (self.length.is_some() || geometry.is_some())
            || kind == AttachmentKind::File && geometry.is_some()
        {
            return Err(mismatch("grant shape"));
        }
        Ok(AttachmentReceipt {
            role: role.to_owned(),
            kind,
            read_only,
            binding: self.binding.clone(),
            identity,
            length,
            geometry,
        })
    }
}

impl AttachmentReceiptSet {
    /// Verifies the complete ordered receipt, including the authenticated
    /// activation identity. An omitted, extra, or downgraded grant is an error.
    pub fn verify(&self, grants: &AttachmentGrantSet) -> Result<(), AttachmentError> {
        grants.validate()?;
        if self.version != grants.version
            || self.activation_id != grants.activation_id
            || self.attachments.len() != grants.attachments.len()
        {
            return Err(AttachmentError::Invalid(
                "attachment receipt set does not match the activation",
            ));
        }
        for (receipt, grant) in self.attachments.iter().zip(&grants.attachments) {
            if receipt.role != grant.role
                || receipt.kind != grant.kind
                || receipt.read_only != grant.read_only
                || receipt.binding != grant.binding
                || receipt.geometry != grant.geometry
                || grant.length.is_some() && receipt.length != grant.length
                || (receipt.kind == AttachmentKind::Directory) != receipt.length.is_none()
                || matches!(&grant.binding, AttachmentBinding::ObjectPinned { identity } if *identity != receipt.identity)
            {
                return Err(AttachmentError::Mismatch {
                    role: grant.role.clone(),
                    field: "receipt",
                });
            }
            if let (Some(geometry), Some(length)) = (receipt.geometry, receipt.length) {
                geometry.validate(length)?;
            }
        }
        Ok(())
    }
}

/// An admitted handle transferred to the device, not a pathname to reopen.
#[derive(MeshPayload)]
pub struct OpenedAttachment {
    pub file: File,
    pub receipt: AttachmentReceipt,
}

impl OpenedAttachment {
    pub fn verify(
        &self,
        role: &str,
        kind: AttachmentKind,
        read_only: bool,
    ) -> Result<(), AttachmentError> {
        let grant = AttachmentGrant {
            role: self.receipt.role.clone(),
            kind: self.receipt.kind,
            read_only: self.receipt.read_only,
            binding: AttachmentBinding::ObjectPinned {
                identity: self.receipt.identity.clone(),
            },
            length: self.receipt.length,
            geometry: self.receipt.geometry,
        };
        if !matches!(&self.receipt.binding, AttachmentBinding::ObjectPinned { identity } if *identity == self.receipt.identity)
        {
            return Err(AttachmentError::Invalid(
                "transferred handle is not object-pinned",
            ));
        }
        grant.verify_opened(&self.file, role, kind, read_only, self.receipt.geometry)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn grant(file: &File, kind: AttachmentKind, read_only: bool) -> AttachmentGrant {
        AttachmentGrant {
            role: "block:distro".to_owned(),
            kind,
            read_only,
            binding: AttachmentBinding::ObjectPinned {
                identity: AttachmentIdentity::query(file).unwrap(),
            },
            length: (kind == AttachmentKind::BlockMedia).then_some(file.metadata().unwrap().len()),
            geometry: (kind == AttachmentKind::BlockMedia).then_some(AttachmentGeometry {
                logical_sector_size: 512,
                physical_sector_size: 4096,
            }),
        }
    }

    #[test]
    fn opened_identity_names_the_handle_not_a_replacement_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.gpt");
        std::fs::write(&path, vec![0; 4096]).unwrap();
        let original = File::open(&path).unwrap();
        let expected = AttachmentIdentity::query(&original).unwrap();
        let copy = original.try_clone().unwrap();
        assert_eq!(AttachmentIdentity::query(&copy).unwrap(), expected);
        std::fs::rename(&path, directory.path().join("authorized.gpt")).unwrap();
        std::fs::write(&path, vec![1; 4096]).unwrap();
        assert_eq!(AttachmentIdentity::query(&original).unwrap(), expected);
        assert_ne!(
            AttachmentIdentity::query(&File::open(&path).unwrap()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::from_slice::<AttachmentIdentity>(&serde_json::to_vec(&expected).unwrap())
                .unwrap(),
            expected
        );
    }

    #[test]
    fn media_admission_rejects_authorize_then_replace_race() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.gpt");
        std::fs::write(&path, vec![0; 4096]).unwrap();
        let authorized = File::open(&path).unwrap();
        let grant = grant(&authorized, AttachmentKind::BlockMedia, true);
        std::fs::rename(&path, directory.path().join("authorized.gpt")).unwrap();
        std::fs::write(&path, vec![1; 4096]).unwrap();
        let substituted = File::open(&path).unwrap();
        let error = grant
            .verify_opened(&substituted, &grant.role, grant.kind, true, grant.geometry)
            .unwrap_err();
        assert!(matches!(
            error,
            AttachmentError::Mismatch {
                field: "opened identity",
                ..
            }
        ));
        grant
            .verify_opened(&authorized, &grant.role, grant.kind, true, grant.geometry)
            .unwrap();
    }

    #[test]
    fn media_admission_checks_mode_kind_size_and_geometry() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4096).unwrap();
        let read_only = File::open(file.path()).unwrap();
        let expected = grant(&read_only, AttachmentKind::BlockMedia, true);
        expected
            .verify_opened(
                &read_only,
                &expected.role,
                expected.kind,
                true,
                expected.geometry,
            )
            .unwrap();
        for (kind, mode, geometry) in [
            (AttachmentKind::File, true, expected.geometry),
            (AttachmentKind::BlockMedia, false, expected.geometry),
            (AttachmentKind::BlockMedia, true, None),
            (
                AttachmentKind::BlockMedia,
                true,
                Some(AttachmentGeometry {
                    logical_sector_size: 4096,
                    physical_sector_size: 4096,
                }),
            ),
        ] {
            assert!(
                expected
                    .verify_opened(&read_only, &expected.role, kind, mode, geometry)
                    .is_err()
            );
        }
        assert!(
            expected
                .verify_opened(
                    file.as_file(),
                    &expected.role,
                    expected.kind,
                    true,
                    expected.geometry
                )
                .is_err()
        );
        file.as_file().set_len(8192).unwrap();
        assert!(
            expected
                .verify_opened(
                    &read_only,
                    &expected.role,
                    expected.kind,
                    true,
                    expected.geometry
                )
                .is_err()
        );
        let writable = grant(file.as_file(), AttachmentKind::BlockMedia, false);
        assert!(
            writable
                .verify_opened(
                    &read_only,
                    &writable.role,
                    writable.kind,
                    false,
                    writable.geometry
                )
                .is_err()
        );
        writable
            .verify_opened(
                file.as_file(),
                &writable.role,
                writable.kind,
                false,
                writable.geometry,
            )
            .unwrap();
    }

    #[test]
    fn geometry_is_bounded_and_sector_aligned() {
        for (logical, physical, length) in [
            (0, 4096, 4096),
            (256, 4096, 4096),
            (513, 4096, 4096),
            (512, 256, 4096),
            (512, 4097, 4096),
            (512, 4096, 0),
            (512, 4096, 4097),
        ] {
            assert!(
                AttachmentGeometry {
                    logical_sector_size: logical,
                    physical_sector_size: physical
                }
                .validate(length)
                .is_err()
            );
        }
    }

    #[test]
    fn receipts_bind_every_field_to_one_activation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4096).unwrap();
        let file = File::open(file.path()).unwrap();
        let grant = grant(&file, AttachmentKind::BlockMedia, true);
        let receipt = grant
            .verify_opened(&file, &grant.role, grant.kind, true, grant.geometry)
            .unwrap();
        let grants = AttachmentGrantSet {
            version: ATTACHMENT_HANDOFF_VERSION,
            activation_id: [1; 16],
            protected_roots: vec![ProtectedRoot {
                path: "protected".to_owned(),
                identity: receipt.identity.clone(),
            }],
            attachments: vec![grant],
        };
        let receipts = AttachmentReceiptSet {
            version: grants.version,
            activation_id: grants.activation_id,
            attachments: vec![receipt.clone()],
        };
        receipts.verify(&grants).unwrap();
        for changed in [
            AttachmentReceipt {
                role: "block:runtime".to_owned(),
                ..receipt.clone()
            },
            AttachmentReceipt {
                kind: AttachmentKind::File,
                ..receipt.clone()
            },
            AttachmentReceipt {
                read_only: false,
                ..receipt.clone()
            },
            AttachmentReceipt {
                binding: AttachmentBinding::LivePath,
                ..receipt.clone()
            },
            AttachmentReceipt {
                length: Some(8192),
                ..receipt.clone()
            },
            AttachmentReceipt {
                geometry: None,
                ..receipt.clone()
            },
            AttachmentReceipt {
                identity: AttachmentIdentity::Unix {
                    device: 0,
                    inode: 0,
                },
                ..receipt.clone()
            },
        ] {
            assert!(
                AttachmentReceiptSet {
                    attachments: vec![changed],
                    ..receipts.clone()
                }
                .verify(&grants)
                .is_err()
            );
        }
        assert!(
            AttachmentReceiptSet {
                version: 2,
                ..receipts.clone()
            }
            .verify(&grants)
            .is_err()
        );
        assert!(
            AttachmentReceiptSet {
                activation_id: [2; 16],
                ..receipts.clone()
            }
            .verify(&grants)
            .is_err()
        );
        assert!(
            AttachmentReceiptSet {
                attachments: vec![],
                ..receipts.clone()
            }
            .verify(&grants)
            .is_err()
        );
        assert!(
            AttachmentReceiptSet {
                attachments: vec![receipt.clone(), receipt],
                ..receipts
            }
            .verify(&grants)
            .is_err()
        );
    }

    #[test]
    fn grant_sets_reject_unknown_versions_duplicate_roles_and_unprotected_launches() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let grant = grant(file.as_file(), AttachmentKind::File, false);
        let mut grants = AttachmentGrantSet {
            version: 1,
            activation_id: [1; 16],
            protected_roots: vec![ProtectedRoot {
                path: "protected".to_owned(),
                identity: AttachmentIdentity::query(file.as_file()).unwrap(),
            }],
            attachments: vec![grant.clone()],
        };
        grants.validate().unwrap();
        grants.version = 2;
        assert!(grants.validate().is_err());
        grants.version = 1;
        grants.activation_id = [0; 16];
        assert!(grants.validate().is_err());
        grants.activation_id = [1; 16];
        grants.attachments.push(grant);
        assert!(grants.validate().is_err());
        grants.attachments.pop();
        grants.protected_roots.clear();
        assert!(grants.validate().is_err());
        assert!(serde_json::from_str::<AttachmentBinding>(r#"{"policy":"object-pinned","identity":{"platform":"unix","device":1,"inode":2},"follow":true}"#).is_err());
    }
}
