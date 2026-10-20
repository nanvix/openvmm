// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Admission of caller-approved host objects before either cold or restored
//! vCPUs run. The worker receives the exact verified handles.

use super::filesystem::EffectiveMicrovmFilesystem;
use super::filesystem::canonical_microvm_filesystem_root;
use super::filesystem::validate_microvm_filesystem_roots;
use crate::Options;
use crate::cli_args::DiskCliKind;
use anyhow::Context;
use openvmm_defs::microvm::MicrovmFilesystemChildKind;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use virtio_resources::attachment::AttachmentBinding;
use virtio_resources::attachment::AttachmentGeometry;
use virtio_resources::attachment::AttachmentGrantSet;
use virtio_resources::attachment::AttachmentIdentity;
use virtio_resources::attachment::AttachmentKind;
use virtio_resources::attachment::AttachmentReceipt;
use virtio_resources::attachment::AttachmentReceiptSet;
use virtio_resources::attachment::OpenedAttachment;
use virtio_resources::fs::microvm::MicrovmFsRoot;

const MAX_HANDOFF_BYTES: u64 = 1024 * 1024;

struct ProtectedObject {
    path: String,
    attachment: SnapshotAttachment,
    identity: AttachmentIdentity,
    _file: File,
}

pub(crate) struct VerifiedAttachments {
    grants: AttachmentGrantSet,
    receipts: HashMap<String, AttachmentReceipt>,
    protected: Vec<ProtectedObject>,
    destination: PathBuf,
    failed: bool,
}

fn open_object(path: &Path, directory: bool, read_only: bool) -> anyhow::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(!directory && !read_only);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::*;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open authorized object {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "authorized object has the wrong opened kind"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        anyhow::ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "authorized object is a reparse point"
        );
    }
    Ok(file)
}

fn reject_ancestor_identity(path: &Path, identity: &AttachmentIdentity) -> anyhow::Result<()> {
    for ancestor in path.ancestors().filter(|path| path.has_root()) {
        let file = open_object(ancestor, true, true)?;
        anyhow::ensure!(
            AttachmentIdentity::query(&file)? != *identity,
            "opened filesystem ancestor overlaps a protected object"
        );
    }
    Ok(())
}

impl VerifiedAttachments {
    pub(super) fn from_options(opt: &Options) -> anyhow::Result<Option<Self>> {
        let Some(path) = &opt.microvm.microvm_attachment_grants else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        fs_err::File::open(path)?
            .take(MAX_HANDOFF_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("failed to read attachment grants")?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_HANDOFF_BYTES,
            "attachment grant handoff exceeds 1 MiB"
        );
        let grants = serde_json::from_slice(&bytes).context("invalid attachment grant handoff")?;
        let destination = opt
            .microvm
            .microvm_attachment_receipts
            .as_ref()
            .context("attachment grants require a receipt destination")?;
        Self::new(grants, destination.clone()).map(Some)
    }
    fn new(grants: AttachmentGrantSet, destination: PathBuf) -> anyhow::Result<Self> {
        grants.validate()?;
        anyhow::ensure!(
            fs_err::symlink_metadata(&destination)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "attachment receipt destination already exists or cannot be inspected"
        );
        let mut protected = Vec::with_capacity(grants.protected_roots.len());
        let slot = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0];
        for root in &grants.protected_roots {
            let path = Path::new(&root.path);
            anyhow::ensure!(
                path.is_absolute() && root.path.len() <= 4096,
                "protected root must be an absolute bounded path"
            );
            let kind = if fs_err::symlink_metadata(path)?.is_dir() {
                MicrovmFilesystemChildKind::Directory
            } else {
                MicrovmFilesystemChildKind::File
            };
            let (canonical, identity_kind, identity) =
                canonical_microvm_filesystem_root(path, kind)?;
            let file = open_object(
                &canonical,
                kind == MicrovmFilesystemChildKind::Directory,
                true,
            )?;
            let actual = AttachmentIdentity::query(&file)?;
            anyhow::ensure!(
                actual == root.identity,
                "protected root opened identity does not match its authorization"
            );
            protected.push(ProtectedObject {
                path: canonical
                    .to_str()
                    .context("protected root path is not UTF-8")?
                    .to_owned(),
                attachment: SnapshotAttachment {
                    stable_id: slot.stable_id.to_owned(),
                    kind: "virtio-fs".to_owned(),
                    required: true,
                    reconnect_policy: "live-revalidate".to_owned(),
                    identity_kind: identity_kind.to_owned(),
                    identity,
                    length: 0,
                    reconnect_timeout_ms: 0,
                },
                identity: actual,
                _file: file,
            });
        }
        Ok(Self {
            grants,
            receipts: HashMap::new(),
            protected,
            destination,
            failed: false,
        })
    }

    pub(super) fn activation_id(&self) -> [u8; 16] {
        self.grants.activation_id
    }

    fn open(
        &mut self,
        role: &str,
        path: &Path,
        kind: AttachmentKind,
        read_only: bool,
        geometry: Option<AttachmentGeometry>,
    ) -> anyhow::Result<OpenedAttachment> {
        let grant = self
            .grants
            .attachments
            .iter()
            .find(|grant| grant.role == role)
            .with_context(|| format!("missing authorization for attachment '{role}'"))?;
        anyhow::ensure!(
            !self.receipts.contains_key(role),
            "attachment role '{role}' was admitted more than once"
        );
        let file = open_object(path, kind == AttachmentKind::Directory, read_only)?;
        let receipt = grant.verify_opened(&file, role, kind, read_only, geometry)?;
        Ok(OpenedAttachment { file, receipt })
    }

    fn check_export(
        &self,
        path: &str,
        attachment: &SnapshotAttachment,
        receipt: &AttachmentReceipt,
    ) -> anyhow::Result<()> {
        for protected in &self.protected {
            anyhow::ensure!(
                receipt.identity != protected.identity,
                "filesystem export aliases a protected root"
            );
            validate_microvm_filesystem_roots(&[
                (path.to_owned(), attachment.clone()),
                (protected.path.clone(), protected.attachment.clone()),
            ])
            .context("filesystem export overlaps a protected root")?;
            if let Some(parent) = Path::new(path).parent() {
                reject_ancestor_identity(parent, &protected.identity)?;
            }
            if let Some(parent) = Path::new(&protected.path).parent() {
                reject_ancestor_identity(parent, &receipt.identity)?;
            }
        }
        let parent = self
            .destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs_err::canonicalize(parent)
            .context("failed to resolve attachment receipt directory")?;
        anyhow::ensure!(
            !parent.starts_with(path),
            "attachment receipts must be outside filesystem exports"
        );
        reject_ancestor_identity(&parent, &receipt.identity)?;
        Ok(())
    }

    pub(super) fn filesystem_roots(
        &mut self,
        filesystem: &EffectiveMicrovmFilesystem,
    ) -> anyhow::Result<Vec<MicrovmFsRoot>> {
        anyhow::ensure!(!self.failed, "attachment admission already failed");
        self.failed = true;
        let roots = if filesystem.config.is_aggregate() {
            filesystem
                .config
                .children
                .iter()
                .zip(&filesystem.children)
                .map(|(child, (path, attachment))| {
                    (
                        format!("{}/{}", filesystem.attachment.stable_id, child.name),
                        path.as_str(),
                        attachment,
                        if child.kind == MicrovmFilesystemChildKind::File {
                            AttachmentKind::File
                        } else {
                            AttachmentKind::Directory
                        },
                        child.access.is_read_only(),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            vec![(
                filesystem.attachment.stable_id.clone(),
                filesystem.root_path.as_str(),
                &filesystem.attachment,
                AttachmentKind::Directory,
                filesystem.config.access.is_read_only(),
            )]
        };
        let roots = roots
            .into_iter()
            .map(|(role, path, attachment, kind, read_only)| {
                let opened = self.open(&role, Path::new(path), kind, read_only, None)?;
                self.check_export(path, attachment, &opened.receipt)?;
                anyhow::ensure!(
                    self.receipts
                        .values()
                        .filter(|receipt| receipt.kind != AttachmentKind::BlockMedia)
                        .all(|receipt| receipt.identity != opened.receipt.identity),
                    "microVM filesystem roots must not alias the same opened object"
                );
                self.receipts.insert(role, opened.receipt.clone());
                Ok(match opened.receipt.binding {
                    AttachmentBinding::ObjectPinned { .. } => MicrovmFsRoot::ObjectPinned(opened),
                    AttachmentBinding::LivePath => MicrovmFsRoot::LivePath(path.to_owned()),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        self.failed = false;
        Ok(roots)
    }

    pub(super) fn block(
        &mut self,
        role: MicrovmSandboxBlockRole,
        kind: &DiskCliKind,
        read_only: bool,
    ) -> anyhow::Result<OpenedAttachment> {
        anyhow::ensure!(!self.failed, "attachment admission already failed");
        self.failed = true;
        let kind = match kind {
            DiskCliKind::DelayDiskWrapper { disk, .. } => disk.as_ref(),
            kind => kind,
        };
        let DiskCliKind::File {
            path,
            create_with_len: None,
            direct: false,
        } = kind
        else {
            anyhow::bail!(
                "verified sandbox media requires an existing plain raw file without direct I/O"
            );
        };
        let (canonical, _, _) =
            canonical_microvm_filesystem_root(path, MicrovmFilesystemChildKind::File)?;
        let role = format!("block:{}", role.as_str());
        // Geometry is derived from the same handle that will back the disk.
        let grant = self
            .grants
            .attachments
            .iter()
            .find(|grant| grant.role == role)
            .with_context(|| format!("missing authorization for attachment '{role}'"))?;
        anyhow::ensure!(
            !self.receipts.contains_key(&role),
            "duplicate block authorization"
        );
        let file = open_object(&canonical, false, read_only)?;
        let geometry = crate::storage_builder::microvm::sandbox_block_geometry(&file)?;
        let receipt = grant.verify_opened(
            &file,
            &role,
            AttachmentKind::BlockMedia,
            read_only,
            Some(geometry),
        )?;
        self.receipts.insert(role, receipt.clone());
        self.failed = false;
        Ok(OpenedAttachment { file, receipt })
    }

    fn receipt_set(&self) -> anyhow::Result<AttachmentReceiptSet> {
        anyhow::ensure!(
            !self.failed,
            "attachment admission failed; no receipt can be published"
        );
        anyhow::ensure!(
            self.receipts.len() == self.grants.attachments.len(),
            "not every attachment grant was consumed"
        );
        let receipts = AttachmentReceiptSet {
            version: self.grants.version,
            activation_id: self.grants.activation_id,
            attachments: self
                .grants
                .attachments
                .iter()
                .map(|grant| {
                    self.receipts
                        .get(&grant.role)
                        .cloned()
                        .with_context(|| format!("attachment '{}' was not opened", grant.role))
                })
                .collect::<anyhow::Result<_>>()?,
        };
        receipts.verify(&self.grants)?;
        Ok(receipts)
    }

    pub(super) fn validate_complete(&self) -> anyhow::Result<()> {
        self.receipt_set().map(|_| ())
    }

    pub(super) fn publish(&self) -> anyhow::Result<()> {
        let receipts = self.receipt_set()?;
        let parent = self
            .destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("failed to stage attachment receipts")?;
        serde_json::to_writer(temporary.as_file_mut(), &receipts)
            .context("failed to encode attachment receipts")?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(&self.destination)
            .map_err(|error| error.error)
            .context("failed to publish attachment receipts")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::filesystem::MicrovmFilesystemPolicyPaths;
    use super::super::filesystem::MicrovmFilesystemRequest;
    use super::super::filesystem::effective_microvm_filesystem;
    use super::*;
    use crate::cli_args::microvm::MicrovmMountChildCli;
    use crate::cli_args::microvm::MicrovmMountCli;
    use clap::Parser as _;
    use openvmm_defs::microvm::MicrovmFilesystemOwner;
    use test_with_tracing::test;
    use virtio_resources::attachment::AttachmentGrant;
    use virtio_resources::attachment::ProtectedRoot;

    struct Fixture {
        directory: tempfile::TempDir,
        root: PathBuf,
        protected: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("share");
            let protected = directory.path().join("private");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&protected).unwrap();
            std::fs::write(root.join("config"), b"approved").unwrap();
            Self {
                directory,
                root,
                protected,
            }
        }

        fn grant(
            path: &Path,
            role: &str,
            kind: AttachmentKind,
            read_only: bool,
        ) -> AttachmentGrant {
            let file = open_object(path, kind == AttachmentKind::Directory, read_only).unwrap();
            AttachmentGrant {
                role: role.to_owned(),
                kind,
                read_only,
                binding: AttachmentBinding::ObjectPinned {
                    identity: AttachmentIdentity::query(&file).unwrap(),
                },
                length: (kind == AttachmentKind::BlockMedia)
                    .then_some(file.metadata().unwrap().len()),
                geometry: (kind == AttachmentKind::BlockMedia).then(|| {
                    crate::storage_builder::microvm::sandbox_block_geometry(&file).unwrap()
                }),
            }
        }

        fn grants(&self, attachments: Vec<AttachmentGrant>) -> AttachmentGrantSet {
            let file = open_object(&self.protected, true, true).unwrap();
            AttachmentGrantSet {
                version: 1,
                activation_id: [7; 16],
                protected_roots: vec![ProtectedRoot {
                    path: self.protected.to_str().unwrap().to_owned(),
                    identity: AttachmentIdentity::query(&file).unwrap(),
                }],
                attachments,
            }
        }

        fn admit(&self, grants: AttachmentGrantSet) -> VerifiedAttachments {
            VerifiedAttachments::new(grants, self.directory.path().join("receipts.json")).unwrap()
        }

        fn filesystem(
            &self,
            restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
        ) -> EffectiveMicrovmFilesystem {
            let mount: MicrovmMountCli = format!("/mnt/share,{},ro", self.root.display())
                .parse()
                .unwrap();
            effective_microvm_filesystem(
                Some(MicrovmFilesystemRequest::Single(&mount)),
                MicrovmFilesystemPolicyPaths::default(),
                MicrovmFilesystemOwner::Vmm,
                restore,
            )
            .unwrap()
            .unwrap()
        }

        fn file_filesystem(&self, path: &Path) -> EffectiveMicrovmFilesystem {
            let children: Vec<MicrovmMountChildCli> = vec![
                format!("config,{},ro,file", path.display())
                    .parse()
                    .unwrap(),
            ];
            effective_microvm_filesystem(
                Some(MicrovmFilesystemRequest::Aggregate {
                    guest_target: "/run/exports",
                    children: &children,
                }),
                MicrovmFilesystemPolicyPaths::default(),
                MicrovmFilesystemOwner::Vmm,
                None,
            )
            .unwrap()
            .unwrap()
        }
    }

    #[test]
    fn cold_and_platform_restore_reject_root_replaced_between_validation_and_open() {
        for restore in [false, true] {
            let fixture = Fixture::new();
            let grants = fixture.grants(vec![Fixture::grant(
                &fixture.root,
                "fs:microvm0",
                AttachmentKind::Directory,
                true,
            )]);
            let contract = super::super::filesystem::tests::dormant_filesystem_contract();
            let filesystem = fixture.filesystem(restore.then_some(&contract));
            let mut admission = fixture.admit(grants);
            std::fs::rename(&fixture.root, fixture.directory.path().join("authorized")).unwrap();
            std::fs::create_dir(&fixture.root).unwrap();
            std::fs::write(fixture.root.join("substitute"), b"wrong object").unwrap();
            let error = admission.filesystem_roots(&filesystem).err().unwrap();
            assert!(
                format!("{error:#}").contains("opened identity"),
                "{error:#}"
            );
            assert!(admission.publish().is_err());
            assert!(!admission.destination.exists());
        }
    }

    #[test]
    fn cold_and_unchanged_aggregate_restore_reject_file_replaced_before_open() {
        for restore in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.root.join("config");
            let grants = fixture.grants(vec![Fixture::grant(
                &path,
                "fs:microvm0/config",
                AttachmentKind::File,
                true,
            )]);
            let children: Vec<MicrovmMountChildCli> = vec![
                format!("config,{},ro,file", path.display())
                    .parse()
                    .unwrap(),
            ];
            let request = MicrovmFilesystemRequest::Aggregate {
                guest_target: "/run/exports",
                children: &children,
            };
            let resolve = |contract| {
                effective_microvm_filesystem(
                    Some(request),
                    MicrovmFilesystemPolicyPaths::default(),
                    MicrovmFilesystemOwner::Vmm,
                    contract,
                )
                .unwrap()
                .unwrap()
            };
            let initial = resolve(None);
            let contract = super::super::filesystem::tests::effective_contract(Some(&initial));
            let filesystem = resolve(restore.then_some(&contract));
            let mut admission = fixture.admit(grants);
            std::fs::rename(&path, fixture.root.join("authorized")).unwrap();
            std::fs::write(&path, b"substitute").unwrap();
            assert!(admission.filesystem_roots(&filesystem).is_err());
            assert!(admission.publish().is_err());
            assert!(!admission.destination.exists());
        }
    }

    #[test]
    fn direct_read_only_media_rejects_replacement_and_wrong_geometry_before_admission() {
        let fixture = Fixture::new();
        let path = fixture.directory.path().join("image.gpt");
        std::fs::write(&path, vec![0; 4096]).unwrap();
        let grant = Fixture::grant(&path, "block:distro", AttachmentKind::BlockMedia, true);
        let mut admission = fixture.admit(fixture.grants(vec![grant]));
        let kind = DiskCliKind::File {
            path: path.clone(),
            create_with_len: None,
            direct: false,
        };
        std::fs::rename(&path, fixture.directory.path().join("authorized.gpt")).unwrap();
        std::fs::write(&path, vec![1; 4096]).unwrap();
        assert!(
            admission
                .block(MicrovmSandboxBlockRole::Distro, &kind, true)
                .is_err()
        );
        assert!(admission.publish().is_err());
        let mut wrong = Fixture::grant(&path, "block:distro", AttachmentKind::BlockMedia, true);
        wrong.geometry.as_mut().unwrap().logical_sector_size = 4096;
        let mut admission = fixture.admit(fixture.grants(vec![wrong]));
        assert!(
            admission
                .block(MicrovmSandboxBlockRole::Distro, &kind, true)
                .is_err()
        );
        assert!(admission.publish().is_err());
        assert!(!admission.destination.exists());
    }

    #[test]
    fn media_receipt_queries_keep_the_admitted_identity_after_path_replacement() {
        let fixture = Fixture::new();
        let path = fixture.directory.path().join("image.gpt");
        std::fs::write(&path, vec![7; 4096]).unwrap();
        let grants = fixture.grants(vec![Fixture::grant(
            &path,
            "block:distro",
            AttachmentKind::BlockMedia,
            true,
        )]);
        let mut admission = fixture.admit(grants.clone());
        let kind = DiskCliKind::File {
            path: path.clone(),
            create_with_len: None,
            direct: false,
        };
        let opened = admission
            .block(MicrovmSandboxBlockRole::Distro, &kind, true)
            .unwrap();
        std::fs::rename(&path, fixture.directory.path().join("authorized.gpt")).unwrap();
        std::fs::write(&path, vec![9; 4096]).unwrap();
        admission.publish().unwrap();
        for _ in 0..2 {
            let receipts: AttachmentReceiptSet =
                serde_json::from_slice(&std::fs::read(&admission.destination).unwrap()).unwrap();
            receipts.verify(&grants).unwrap();
            assert_eq!(
                receipts.attachments[0].identity,
                AttachmentIdentity::query(&opened.file).unwrap()
            );
            assert_ne!(
                receipts.attachments[0].identity,
                AttachmentIdentity::query(&File::open(&path).unwrap()).unwrap()
            );
        }
        assert!(admission.publish().is_err());
    }

    #[test]
    fn cold_and_platform_restore_publish_actual_handle_receipts_for_one_activation() {
        for restore in [false, true] {
            let fixture = Fixture::new();
            let grants = fixture.grants(vec![Fixture::grant(
                &fixture.root,
                "fs:microvm0",
                AttachmentKind::Directory,
                true,
            )]);
            let contract = super::super::filesystem::tests::dormant_filesystem_contract();
            let filesystem = fixture.filesystem(restore.then_some(&contract));
            let mut admission = fixture.admit(grants.clone());
            let roots = admission.filesystem_roots(&filesystem).unwrap();
            let MicrovmFsRoot::ObjectPinned(opened) = &roots[0] else {
                panic!("pinned grant was downgraded")
            };
            assert_eq!(
                AttachmentIdentity::query(&opened.file).unwrap(),
                opened.receipt.identity
            );
            admission.publish().unwrap();
            let receipts: AttachmentReceiptSet =
                serde_json::from_slice(&std::fs::read(&admission.destination).unwrap()).unwrap();
            receipts.verify(&grants).unwrap();
            assert_eq!(receipts.activation_id, admission.activation_id());
            assert!(admission.publish().is_err());
        }
    }

    #[test]
    fn protected_roots_reject_equal_parent_child_and_identity_alias_exports() {
        let fixture = Fixture::new();
        let child = fixture.protected.join("child");
        std::fs::create_dir(&child).unwrap();
        let parent = fixture.directory.path().to_owned();
        for path in [&fixture.protected, &child, &parent] {
            let grant = Fixture::grant(path, "fs:microvm0", AttachmentKind::Directory, true);
            let mut admission = fixture.admit(fixture.grants(vec![grant]));
            let mount: MicrovmMountCli =
                format!("/mnt/share,{},ro", path.display()).parse().unwrap();
            let filesystem = effective_microvm_filesystem(
                Some(MicrovmFilesystemRequest::Single(&mount)),
                MicrovmFilesystemPolicyPaths::default(),
                MicrovmFilesystemOwner::Vmm,
                None,
            )
            .unwrap()
            .unwrap();
            assert!(admission.filesystem_roots(&filesystem).is_err());
            assert!(admission.publish().is_err());
            assert!(!admission.destination.exists());
        }
        let secret = fixture.protected.join("secret");
        let alias = fixture.root.join("alias");
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::hard_link(&secret, &alias).unwrap();
        let mut grants = fixture.grants(vec![Fixture::grant(
            &alias,
            "fs:microvm0/config",
            AttachmentKind::File,
            true,
        )]);
        let file = File::open(&secret).unwrap();
        grants.protected_roots.push(ProtectedRoot {
            path: secret.to_str().unwrap().to_owned(),
            identity: AttachmentIdentity::query(&file).unwrap(),
        });
        let mut admission = fixture.admit(grants);
        assert!(
            admission
                .filesystem_roots(&fixture.file_filesystem(&alias))
                .is_err()
        );
        assert!(admission.publish().is_err());
    }

    #[test]
    fn explicit_live_path_grants_are_receipted_without_becoming_pinned() {
        let fixture = Fixture::new();
        let path = fixture.root.join("config");
        let mut grant = Fixture::grant(&path, "fs:microvm0/config", AttachmentKind::File, true);
        grant.binding = AttachmentBinding::LivePath;
        let grants = fixture.grants(vec![grant]);
        let mut admission = fixture.admit(grants.clone());
        let roots = admission
            .filesystem_roots(&fixture.file_filesystem(&path))
            .unwrap();
        assert!(matches!(&roots[0], MicrovmFsRoot::LivePath(_)));
        admission.receipt_set().unwrap().verify(&grants).unwrap();
    }

    #[test]
    fn opened_root_aliases_are_rejected_even_when_legacy_identity_labels_differ() {
        use openvmm_defs::microvm::MicrovmFilesystemAccess;
        use openvmm_defs::microvm::MicrovmFilesystemChildConfig;
        let fixture = Fixture::new();
        let path = fixture.root.join("config");
        let alias = fixture.root.join("alias");
        std::fs::hard_link(&path, &alias).unwrap();
        let grants = fixture.grants(vec![
            Fixture::grant(&path, "fs:microvm0/config", AttachmentKind::File, true),
            Fixture::grant(&alias, "fs:microvm0/alias", AttachmentKind::File, true),
        ]);
        let mut filesystem = fixture.file_filesystem(&path);
        filesystem.config.children.push(
            MicrovmFilesystemChildConfig::new_file(
                "alias".to_owned(),
                MicrovmFilesystemAccess::ReadOnly,
            )
            .unwrap(),
        );
        let mut legacy = filesystem.children[0].1.clone();
        legacy.identity.push(1);
        filesystem
            .children
            .push((alias.to_str().unwrap().to_owned(), legacy));
        let mut admission = fixture.admit(grants);
        let error = admission.filesystem_roots(&filesystem).err().unwrap();
        assert!(format!("{error:#}").contains("same opened object"));
        assert!(admission.publish().is_err());
        assert!(!admission.destination.exists());
    }

    #[test]
    fn missing_extra_and_wrong_mode_grants_never_publish_receipts() {
        let fixture = Fixture::new();
        let filesystem = fixture.filesystem(None);
        let mut admission = fixture.admit(fixture.grants(Vec::new()));
        assert!(admission.filesystem_roots(&filesystem).is_err());
        assert!(admission.publish().is_err());
        assert!(!admission.destination.exists());
        let mut wrong_mode = Fixture::grant(
            &fixture.root,
            "fs:microvm0",
            AttachmentKind::Directory,
            true,
        );
        wrong_mode.read_only = false;
        let mut admission = fixture.admit(fixture.grants(vec![wrong_mode]));
        assert!(admission.filesystem_roots(&filesystem).is_err());
        let extra = Fixture::grant(
            &fixture.root,
            "fs:microvm0/extra",
            AttachmentKind::Directory,
            true,
        );
        let mut admission = fixture.admit(fixture.grants(vec![
            Fixture::grant(
                &fixture.root,
                "fs:microvm0",
                AttachmentKind::Directory,
                true,
            ),
            extra,
        ]));
        admission.filesystem_roots(&filesystem).unwrap();
        assert!(admission.publish().is_err());
        assert!(!admission.destination.exists());
    }

    #[test]
    fn a_file_replaced_by_a_directory_and_a_substituted_protected_root_fail_closed() {
        let fixture = Fixture::new();
        let path = fixture.root.join("config");
        let grants = fixture.grants(vec![Fixture::grant(
            &path,
            "fs:microvm0/config",
            AttachmentKind::File,
            true,
        )]);
        let filesystem = fixture.file_filesystem(&path);
        let mut admission = fixture.admit(grants);
        std::fs::rename(&path, fixture.root.join("authorized")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(admission.filesystem_roots(&filesystem).is_err());
        assert!(admission.publish().is_err());
        let grants = fixture.grants(Vec::new());
        std::fs::rename(
            &fixture.protected,
            fixture.directory.path().join("authorized-private"),
        )
        .unwrap();
        std::fs::create_dir(&fixture.protected).unwrap();
        assert!(
            VerifiedAttachments::new(grants, fixture.directory.path().join("receipts.json"))
                .is_err()
        );
    }

    #[test]
    fn the_opt_in_handoff_requires_authentication_and_a_receipt_sink() {
        for args in [
            vec!["--microvm-attachment-grants", "grants.json"],
            vec!["--microvm-attachment-receipts", "receipts.json"],
            vec![
                "--microvm-attachment-grants",
                "grants.json",
                "--microvm-attachment-receipts",
                "receipts.json",
            ],
        ] {
            assert!(
                Options::try_parse_from(
                    ["openvmm", "--machine", "microvm"].into_iter().chain(args)
                )
                .is_err()
            );
        }
        Options::try_parse_from(["openvmm", "--machine", "microvm"])
            .unwrap()
            .validate_microvm_options()
            .unwrap();
    }
}
