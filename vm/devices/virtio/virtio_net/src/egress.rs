// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress policy enforcement for virtio-net transmit.
//!
//! [`NicBuilder::egress_policy`] configures a run-scoped [`EgressPolicy`]. The
//! device validates the policy and installs it on the endpoint, which applies
//! it to the bytes it transmits, and checks the prefix of each guest frame
//! against the policy before handing the frame to the endpoint.

use crate::Device;
use crate::NicBuilder;
use crate::TxPacketError;
use crate::Worker;
use anyhow::Context as _;
use net_backend::Endpoint;
use net_backend::QueueConfig;
use net_backend_resources::egress::EgressPolicy;

pub(crate) enum RestorePolicyState {
    Disabled,
    Held,
    Installed,
    Failed,
}

impl RestorePolicyState {
    pub(crate) fn ensure_ready(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self, Self::Disabled | Self::Installed),
            "virtio-net restore policy barrier has not completed"
        );
        Ok(())
    }
}

impl NicBuilder {
    pub fn egress_policy(mut self, egress_policy: EgressPolicy) -> Self {
        self.egress_policy = Some(egress_policy);
        self
    }

    /// Holds the endpoint deny-all until fallible restore startup prepares
    /// its listeners and installs `policy` under the VMM's VP barrier.
    pub fn restore_egress_policy(mut self, policy: EgressPolicy) -> Self {
        self.egress_policy = Some(policy);
        self.restore_policy = true;
        self
    }

    /// Validates the configured egress policy and installs it on `endpoint`.
    pub(crate) fn configure_egress(
        &self,
        mut endpoint: Box<dyn Endpoint>,
    ) -> anyhow::Result<Box<dyn Endpoint>> {
        anyhow::ensure!(
            !self.restore_policy || (self.save_restore.is_some() && self.egress_policy.is_some()),
            "a virtio-net restore policy barrier requires saved-state support and a policy"
        );
        if let Some(policy) = &self.egress_policy {
            policy
                .validate()
                .context("network device received an invalid egress policy")?;
            endpoint
                .set_egress_policy(if self.restore_policy {
                    policy.deny_all()?
                } else {
                    policy.clone()
                })
                .with_context(|| {
                    format!(
                        "network backend '{}' rejected egress policy",
                        endpoint.endpoint_type()
                    )
                })?;
        }
        Ok(endpoint)
    }
}

impl Device {
    pub(crate) async fn prepare_restore_policy(&mut self) -> anyhow::Result<()> {
        if !matches!(self.restore_policy, RestorePolicyState::Held) {
            return self.restore_policy.ensure_ready();
        }
        // Failure or cancellation is terminal. A later start must not release
        // the VP barrier with partly prepared listeners or an old policy.
        self.restore_policy = RestorePolicyState::Failed;
        anyhow::ensure!(
            !self.coordinator.is_running() && !self.coordinator.has_state(),
            "cannot prepare a restore policy after virtio-net queues start"
        );
        let coordinator = self.coordinator.task_mut();
        let mut queues = Vec::new();
        coordinator
            .endpoint
            .get_queues(
                vec![QueueConfig {
                    driver: Box::new(self.adapter.driver.clone()),
                }],
                None,
                &mut queues,
            )
            .await
            .context("failed to prepare restored network listeners")?;
        anyhow::ensure!(
            queues.len() == 1,
            "restored network endpoint did not prepare exactly one queue"
        );
        drop(queues);
        coordinator.endpoint.stop().await;
        let policy = self
            .adapter
            .egress_policy
            .as_ref()
            .context("restored network policy is missing")?;
        coordinator
            .endpoint
            .set_egress_policy(policy.clone())
            .context("failed to install restored network policy")?;
        self.restore_policy = RestorePolicyState::Installed;
        Ok(())
    }
}

impl Worker {
    /// Checks the prefix of a guest TX frame against the egress policy.
    pub(crate) fn authorize_egress(
        &self,
        packet_prefix: &[u8],
        packet_len: u32,
    ) -> Result<(), TxPacketError> {
        if let Some(policy) = &self.egress_policy {
            policy
                .authorize_frame(packet_prefix, packet_len as usize)
                .map_err(TxPacketError::EgressDenied)?;
        }
        Ok(())
    }
}
