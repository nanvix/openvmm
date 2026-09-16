// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bounded local outcome reporting for direct microVM runs.

use crate::cli_args::MachineProfileCli;
use crate::cli_args::MicrovmLifecycleCli;
use crate::cli_args::MicrovmNetworkActionCli;
use crate::cli_args::Options;
use anyhow::Context as _;
use serde::Serialize;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

#[derive(Serialize)]
struct MicrovmOutcomeReport {
    schema_version: u32,
    instance_id: String,
    backend: String,
    outcome: Outcome,
    network_policy: NetworkPolicyOutcome,
    teardown: TeardownOutcome,
}

#[derive(Serialize)]
struct Outcome {
    operation: &'static str,
    category: &'static str,
    status_code: i32,
}

#[derive(Serialize)]
struct NetworkPolicyOutcome {
    status: &'static str,
    status_code: u32,
    mode: &'static str,
    allow_rule_count: usize,
    deny_rule_count: usize,
    host_loopback: &'static str,
}

#[derive(Serialize)]
struct TeardownOutcome {
    guest_workload_stopped: bool,
    vm_stopped: bool,
    openvmm_process_terminated: bool,
    virtiofs_released: bool,
    network_released: bool,
    temporary_storage_removed: bool,
    control_channels_closed: bool,
}

pub(crate) struct MicrovmReportPlan {
    path: PathBuf,
    instance_id: String,
    backend: String,
    operation: &'static str,
    policy_requested: bool,
    policy_mode: &'static str,
    allow_rule_count: usize,
    deny_rule_count: usize,
    host_loopback: &'static str,
}

impl MicrovmReportPlan {
    pub(crate) fn from_options(opt: &Options) -> anyhow::Result<Option<Self>> {
        let Some(path) = &opt.microvm_report else {
            return Ok(None);
        };
        anyhow::ensure!(
            opt.machine == MachineProfileCli::Microvm,
            "--microvm-report requires a microVM machine"
        );
        let path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()
                .context("failed to resolve the microVM report path")?
                .join(path)
        };
        validate_destination(&path)?;

        let mut instance = [0u8; 16];
        getrandom::fill(&mut instance).context("failed to generate a report instance ID")?;
        let instance_id = instance
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let backend = opt
            .hypervisor
            .as_deref()
            .and_then(|value| value.split(':').next())
            .unwrap_or("auto");
        anyhow::ensure!(
            matches!(backend, "auto" | "kvm" | "mshv" | "whp"),
            "microVM report backend category is unsupported"
        );
        let operation = if opt.restore_snapshot.is_some() {
            "restore"
        } else if opt.snapshot_destination.is_some() {
            "capture"
        } else if opt.microvm_lifecycle == Some(MicrovmLifecycleCli::Managed) {
            "managed"
        } else {
            "run"
        };
        let policy_requested = !opt.net.is_empty()
            || opt.network_profile.is_some()
            || opt.network_egress.is_some()
            || opt.network_ingress.is_some()
            || !opt.network_egress_allow.is_empty()
            || !opt.network_egress_deny.is_empty()
            || !opt.allow_host.is_empty()
            || !opt.block_host.is_empty()
            || !opt.allow_endpoint.is_empty()
            || opt.host_loopback.is_some()
            || opt.network_proxy.is_some()
            || !opt.host_loopback_forward.is_empty();
        let policy_mode =
            if !opt.network_egress_allow.is_empty() || !opt.network_egress_deny.is_empty() {
                "rules"
            } else if !opt.allow_host.is_empty() {
                "allow-list"
            } else if !opt.block_host.is_empty() {
                "block-list"
            } else if !opt.allow_endpoint.is_empty() {
                "endpoint"
            } else if opt.network_egress == Some(MicrovmNetworkActionCli::Deny) {
                "deny-all"
            } else if policy_requested {
                "allow-all"
            } else {
                "none"
            };
        let allow_rule_count =
            opt.network_egress_allow.len() + opt.allow_host.len() + opt.allow_endpoint.len();
        let deny_rule_count = opt.network_egress_deny.len() + opt.block_host.len();
        let host_loopback = if policy_requested {
            match opt.host_loopback.unwrap_or(MicrovmNetworkActionCli::Allow) {
                MicrovmNetworkActionCli::Allow => "allow",
                MicrovmNetworkActionCli::Deny => "deny",
            }
        } else {
            "none"
        };
        Ok(Some(Self {
            path,
            instance_id,
            backend: backend.to_owned(),
            operation,
            policy_requested,
            policy_mode,
            allow_rule_count,
            deny_rule_count,
            host_loopback,
        }))
    }

    pub(crate) fn write(self, result: &anyhow::Result<i32>) -> anyhow::Result<()> {
        let teardown_failure = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<crate::vm_controller::MicrovmTeardownError>());
        let (category, status_code) = match (result, teardown_failure) {
            (_, Some(_)) => ("teardown-failure", 1),
            (Ok(0), None) => ("success", 0),
            (Ok(code), None) => ("guest-exit", *code),
            (Err(_), None) => ("vmm-failure", 1),
        };
        let policy_status = if !self.policy_requested {
            ("not-requested", 0)
        } else if result.is_ok() || teardown_failure.is_some() {
            ("applied", 0)
        } else {
            ("failed", 1)
        };
        let teardown = teardown_failure.map(|error| error.0).unwrap_or(
            crate::vm_controller::MicrovmTeardownStatus {
                vm_worker_stopped: true,
                auxiliary_workers_stopped: true,
            },
        );
        let report = MicrovmOutcomeReport {
            schema_version: 1,
            instance_id: self.instance_id,
            backend: self.backend,
            outcome: Outcome {
                operation: self.operation,
                category,
                status_code,
            },
            network_policy: NetworkPolicyOutcome {
                status: policy_status.0,
                status_code: policy_status.1,
                mode: self.policy_mode,
                allow_rule_count: self.allow_rule_count,
                deny_rule_count: self.deny_rule_count,
                host_loopback: self.host_loopback,
            },
            teardown: TeardownOutcome {
                guest_workload_stopped: teardown.vm_worker_stopped,
                vm_stopped: teardown.vm_worker_stopped,
                openvmm_process_terminated: true,
                virtiofs_released: teardown.vm_worker_stopped,
                network_released: teardown.vm_worker_stopped,
                temporary_storage_removed: true,
                control_channels_closed: teardown.vm_worker_stopped
                    && teardown.auxiliary_workers_stopped,
            },
        };
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("failed to create the microVM report staging file")?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), &report)
            .context("failed to encode the microVM report")?;
        temporary
            .as_file_mut()
            .write_all(b"\n")
            .context("failed to terminate the microVM report")?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("failed to flush the microVM report")?;
        temporary
            .persist_noclobber(&self.path)
            .map_err(|error| error.error)
            .context("failed to publish the microVM report")?;
        Ok(())
    }
}

fn validate_destination(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(!path.as_os_str().is_empty(), "microVM report path is empty");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = fs_err::symlink_metadata(parent).with_context(|| {
        format!(
            "failed to inspect the microVM report parent {}",
            parent.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "microVM report parent is not a plain directory"
    );
    anyhow::ensure!(
        fs_err::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "microVM report destination already exists or cannot be inspected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm_controller::MicrovmTeardownError;
    use crate::vm_controller::MicrovmTeardownStatus;
    use serde_json::Value;
    use test_with_tracing::test;

    fn test_plan(path: PathBuf) -> MicrovmReportPlan {
        MicrovmReportPlan {
            path,
            instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
            backend: "whp".to_owned(),
            operation: "run",
            policy_requested: true,
            policy_mode: "rules",
            allow_rule_count: 2,
            deny_rule_count: 1,
            host_loopback: "deny",
        }
    }

    fn read_report(path: &Path) -> Value {
        serde_json::from_str(&fs_err::read_to_string(path).expect("the report should be readable"))
            .expect("the report should be valid JSON")
    }

    #[test]
    fn report_has_an_exact_bounded_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome.json");
        test_plan(path.clone()).write(&Ok(23)).unwrap();

        let report = read_report(&path);
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["instance_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(report["backend"], "whp");
        assert_eq!(
            report["outcome"],
            serde_json::json!({
                "operation": "run",
                "category": "guest-exit",
                "status_code": 23,
            })
        );
        assert_eq!(
            report["network_policy"],
            serde_json::json!({
                "status": "applied",
                "status_code": 0,
                "mode": "rules",
                "allow_rule_count": 2,
                "deny_rule_count": 1,
                "host_loopback": "deny",
            })
        );
        assert_eq!(
            report["teardown"],
            serde_json::json!({
                "guest_workload_stopped": true,
                "vm_stopped": true,
                "openvmm_process_terminated": true,
                "virtiofs_released": true,
                "network_released": true,
                "temporary_storage_removed": true,
                "control_channels_closed": true,
            })
        );
        assert_eq!(report.as_object().unwrap().len(), 6);

        let encoded = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "command",
            "environment",
            "path",
            "destination",
            "proxy",
            "output",
            "credential",
            "error",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "report exposed forbidden field category {forbidden:?}"
            );
        }
    }

    #[test]
    fn teardown_failures_are_structured() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome.json");
        let result = Err(anyhow::Error::new(MicrovmTeardownError(
            MicrovmTeardownStatus {
                vm_worker_stopped: false,
                auxiliary_workers_stopped: true,
            },
        )));
        test_plan(path.clone()).write(&result).unwrap();

        let report = read_report(&path);
        assert_eq!(report["outcome"]["category"], "teardown-failure");
        assert_eq!(report["outcome"]["status_code"], 1);
        assert_eq!(report["network_policy"]["status"], "applied");
        assert_eq!(report["teardown"]["guest_workload_stopped"], false);
        assert_eq!(report["teardown"]["vm_stopped"], false);
        assert_eq!(report["teardown"]["virtiofs_released"], false);
        assert_eq!(report["teardown"]["network_released"], false);
        assert_eq!(report["teardown"]["temporary_storage_removed"], true);
        assert_eq!(report["teardown"]["control_channels_closed"], false);
    }

    #[test]
    fn destination_must_not_exist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome.json");
        fs_err::write(&path, b"existing").unwrap();
        let error = validate_destination(&path).unwrap_err();
        assert!(error.to_string().contains("destination already exists"));
    }
}
