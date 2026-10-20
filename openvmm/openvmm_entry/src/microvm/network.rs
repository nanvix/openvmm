// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM static network identity and egress policy.

use crate::Options;
use crate::VmResources;
use crate::cli_args;
use crate::cli_args::EndpointConfigCli;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use net_backend_resources::consomme::static_ipv6::StaticIpv6Config;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::NetEndpointHandleKind;

const MICROVM_NETWORK_STABLE_ID: &str = "net:microvm0";

#[derive(Clone)]
pub(super) struct EffectiveMicrovmNetwork {
    pub(super) config: openvmm_defs::microvm::MicrovmNetworkConfig,
    pub(super) policy: net_backend_resources::egress::EgressPolicy,
    pub(super) attachment: openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    pub(super) restore_policy_barrier: bool,
}

fn microvm_network_attachment() -> openvmm_helpers::snapshot::microvm::SnapshotAttachment {
    openvmm_helpers::snapshot::microvm::SnapshotAttachment {
        stable_id: MICROVM_NETWORK_STABLE_ID.to_owned(),
        kind: "virtio-net".to_owned(),
        required: false,
        reconnect_policy: "recreate-endpoint".to_owned(),
        identity_kind: "user-mode-nat".to_owned(),
        identity: b"consomme".to_vec(),
        length: 0,
        reconnect_timeout_ms: 0,
    }
}

fn microvm_network_from_snapshot(
    saved: &openvmm_helpers::snapshot::microvm::SnapshotMicrovmNetwork,
) -> anyhow::Result<openvmm_defs::microvm::MicrovmNetworkConfig> {
    openvmm_helpers::snapshot::microvm::microvm_network_config_from_snapshot(saved)
}

pub(super) fn effective_microvm_network(
    opt: &Options,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmNetwork>> {
    let requested = match opt.net.as_slice() {
        [] => None,
        [network] => match &network.endpoint {
            EndpointConfigCli::Microvm(config) => Some(config.clone()),
            _ => anyhow::bail!("microVM --net was not validated as a static IPv4 identity"),
        },
        _ => anyhow::bail!("microVM permits at most one virtio-net device"),
    };
    let Some(restore) = restore else {
        return requested
            .map(|config| {
                let policy = if opt.microvm.platform_template {
                    config.deny_all_policy()?
                } else {
                    opt.microvm_egress_policy(&config)?
                };
                Ok(EffectiveMicrovmNetwork {
                    config,
                    policy,
                    attachment: microvm_network_attachment(),
                    restore_policy_barrier: false,
                })
            })
            .transpose();
    };
    openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(restore)?;
    let platform_template = restore.platform_template_version != 0;
    anyhow::ensure!(
        platform_template || requested.is_none(),
        "restore takes microVM network addressing from saved state"
    );

    let has_device = restore
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_NETWORK_STABLE_ID);
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID);
    anyhow::ensure!(
        has_device == restore.microvm_network.is_some() && has_device == saved_attachment.is_some(),
        "snapshot microVM network device, identity, and attachment inventories disagree"
    );
    let Some(saved) = restore.microvm_network.as_ref() else {
        anyhow::ensure!(
            requested.is_none()
                && opt.microvm.network_profile.is_none()
                && opt.microvm.net_tap.is_none()
                && opt.microvm.network_egress.is_none()
                && opt.microvm.network_ingress.is_none()
                && opt.microvm.allow_host.is_empty()
                && opt.microvm.block_host.is_empty()
                && opt.microvm.allow_endpoint.is_empty()
                && opt.microvm.network_egress_allow.is_empty()
                && opt.microvm.network_egress_deny.is_empty()
                && opt.microvm.host_loopback.is_none()
                && opt.microvm.network_proxy.is_none()
                && opt.microvm.host_loopback_forward.is_empty(),
            "restore-time network resources cannot be added to a snapshot without a NIC"
        );
        return Ok(None);
    };
    anyhow::ensure!(
        saved.profile == openvmm_defs::microvm::MicrovmNetworkProfile::Portable.as_str(),
        "snapshot microVM network profile is unsupported"
    );
    let config = microvm_network_from_snapshot(saved)?;
    let policy = if platform_template {
        let closed = config.deny_all_policy()?;
        anyhow::ensure!(
            saved.egress_policy_encoding_version == closed.encoding_version(),
            "platform-template snapshot egress policy encoding version is not canonical"
        );
        openvmm_helpers::snapshot::microvm::validate_microvm_network_policy(saved, &closed)?;
        anyhow::ensure!(
            requested
                .as_ref()
                .is_none_or(|requested| requested == &config),
            "requested network address identity or address family does not match the fixed platform-template NIC"
        );
        anyhow::ensure!(
            opt.microvm.network_profile.is_some() || !opt.microvm_network_policy_requested(),
            "platform-template restore network policy requires --network-profile portable"
        );
        opt.microvm_template_egress_policy(&config)?
    } else {
        anyhow::ensure!(
            opt.microvm.network_profile
                == Some(cli_args::microvm::MicrovmNetworkProfileCli::Portable),
            "networked microVM snapshot restore requires --network-profile portable"
        );
        anyhow::ensure!(
            opt.microvm.network_ingress != Some(cli_args::microvm::MicrovmNetworkActionCli::Allow)
                && opt.microvm.host_loopback
                    != Some(cli_args::microvm::MicrovmNetworkActionCli::Allow)
                && opt.microvm.host_loopback_forward.is_empty(),
            "only a platform-template snapshot supports restore-time ingress or host-loopback listeners"
        );
        let policy = opt.microvm_egress_policy(&config)?;
        openvmm_helpers::snapshot::microvm::validate_microvm_network_policy(saved, &policy)?;
        policy
    };
    policy.validate()?;
    let attachment = microvm_network_attachment();
    anyhow::ensure!(
        Some(&attachment) == saved_attachment,
        "restore-time network endpoint does not match the snapshot attachment"
    );
    Ok(Some(EffectiveMicrovmNetwork {
        config,
        policy,
        attachment,
        restore_policy_barrier: platform_template,
    }))
}

/// Returns the ingress for which the guest announces its listeners under
/// `policy`: private-network ingress, whose listeners also admit the host with
/// explicit host-loopback access, or else host-loopback ingress alone.
pub(super) fn microvm_ingress(
    policy: &net_backend_resources::egress::EgressPolicy,
) -> Option<openvmm_defs::microvm::MicrovmIngress> {
    use net_backend_resources::egress::EgressAction;
    use openvmm_defs::microvm::MicrovmIngress;
    if policy.ingress_action() == EgressAction::Allow {
        Some(MicrovmIngress::PrivateNetworks)
    } else if policy.host_loopback_ingress_action() == EgressAction::Allow {
        Some(MicrovmIngress::HostLoopback)
    } else {
        None
    }
}

pub(super) fn microvm_network_endpoint(
    network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    policy: &net_backend_resources::egress::EgressPolicy,
    loopback_forwards: &[cli_args::microvm::MicrovmLoopbackForwardCli],
    private_network_ingress: Option<net_backend_resources::consomme::PrivateNetworkIngress>,
    _resources: &mut VmResources,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    let ports = loopback_forwards
        .iter()
        .map(|forward| net_backend_resources::consomme::HostPortConfig {
            protocol: match forward.protocol {
                cli_args::microvm::MicrovmLoopbackForwardProtocol::Tcp => {
                    net_backend_resources::consomme::HostPortProtocol::Tcp
                }
                cli_args::microvm::MicrovmLoopbackForwardProtocol::Udp => {
                    net_backend_resources::consomme::HostPortProtocol::Udp
                }
            },
            host_address: Some(net_backend_resources::consomme::HostIpAddress::Ipv4(
                std::net::Ipv4Addr::LOCALHOST,
            )),
            host_port: net_backend_resources::consomme::HostPort::Fixed(forward.host_port),
            guest_port: forward.guest_port,
        })
        .collect();
    let allow_host_loopback =
        policy.host_loopback_action() == net_backend_resources::egress::EgressAction::Allow;
    Ok(net_backend_resources::consomme::ConsommeHandle {
        cidr: None,
        static_ipv4: Some(StaticIpv4Config {
            guest_ipv4: network.guest_ipv4,
            prefix_length: network.prefix_length,
            gateway_ipv4: network.derived_gateway_ipv4,
            gateway_mac: network.gateway_mac,
        }),
        static_ipv6: network.ipv6.map(|ipv6| StaticIpv6Config {
            guest_ipv6: ipv6.guest_ipv6,
            prefix_length: ipv6.prefix_length,
            gateway_ipv6: ipv6.derived_gateway_ipv6,
        }),
        ports,
        recv: None,
        allow_host_local_access: Some(allow_host_loopback),
        map_gateway_to_host_loopback: Some(allow_host_loopback),
        gateway_loopback_proxy_port: policy.proxy_endpoint().map(|endpoint| endpoint.port()),
        private_network_ingress,
        host_loopback_ingress: microvm_ingress(policy)
            == Some(openvmm_defs::microvm::MicrovmIngress::HostLoopback),
    }
    .into_resource())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::microvm::restore::align_legacy_network_policy_contract;
    use clap::Parser as _;
    use openvmm_defs::microvm::build_microvm_command_line;
    use test_with_tracing::test;

    pub(crate) fn network_contract() -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract
    {
        network_contract_for(&"10.0.0.2/24".parse().unwrap())
    }

    fn network_contract_for(
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        network_contract_with_topology(network, false)
    }

    fn network_contract_with_topology(
        network: &openvmm_defs::microvm::MicrovmNetworkConfig,
        template: bool,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let policy = if template {
            network.deny_all_policy().unwrap()
        } else {
            network
                .bind_egress_policy(
                    net_backend_resources::egress::EgressPolicyMode::TcpEndpoints(vec![
                        "10.0.0.9:8443".parse().unwrap(),
                        "192.0.2.7:443".parse().unwrap(),
                        "10.0.0.9:443".parse().unwrap(),
                    ]),
                )
                .unwrap()
        };
        let source_hypervisor = if cfg!(windows) { "whp" } else { "kvm" };
        let irq = openvmm_defs::microvm::microvm_virtio_net_irq(Some(source_hypervisor)).unwrap();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        if template {
            command_line.push_str(" nvx_snapshot_tier=platform ");
            command_line
                .push_str(openvmm_defs::microvm::MICROVM_PLATFORM_TEMPLATE_COMMAND_LINE_TOKEN);
        }
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            Some((network, irq, policy.gateway_dns_server(), None)),
            false,
            &[],
            false,
            false,
            &[],
        )
        .unwrap();
        openvmm_helpers::snapshot::microvm::microvm_machine_contract(
            source_hypervisor,
            openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            Some((network, &policy, microvm_network_attachment())),
            false,
            None,
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            [
                "partition",
                "vmtime",
                "pic",
                "ioapic",
                "pit",
                "rtc",
                "microvm-portb",
                "microvm-shutdown",
                "microvm-snapshot-request",
                "virtio-net-3489660928",
            ]
            .map(str::to_owned)
            .to_vec(),
            crate::microvm::restore::tests::test_time_contract(),
            crate::microvm::restore::tests::test_cpu_profile(),
        )
        .unwrap()
    }

    fn template_options(arguments: &[&str]) -> Options {
        let options = Options::try_parse_from(
            [
                "openvmm",
                "--machine",
                "microvm",
                "--restore-snapshot",
                "snapshot",
            ]
            .into_iter()
            .chain(arguments.iter().copied()),
        )
        .unwrap();
        options.validate_microvm_options().unwrap();
        options
    }

    #[test]
    fn platform_template_restores_two_policies_or_no_network_on_the_same_nic() {
        use net_backend_resources::egress::EgressAction;

        let config = "10.0.0.2/24".parse().unwrap();
        let contract = network_contract_with_topology(&config, true);
        assert_eq!(contract.platform_template_version, 1);
        assert!(!contract.effective_command_line.contains("virtnet_"));
        let arguments: &[&[&str]] = &[
            &[],
            &["--network-profile", "portable"],
            &[
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "192.0.2.0/24:tcp:443",
                "--network-egress-deny",
                "192.0.2.9",
                "--host-loopback",
                "deny",
                "--network-proxy",
                "10.0.0.1:8443",
            ],
            &[
                "--network-profile",
                "portable",
                "--network-egress",
                "deny",
                "--network-egress-allow",
                "2001:db8::/64:udp:53",
                "--host-loopback",
                "allow",
                "--host-loopback-forward",
                "tcp:28443:443",
                "--network-ingress",
                "allow",
            ],
        ];
        let networks = arguments
            .iter()
            .map(|arguments| {
                effective_microvm_network(&template_options(arguments), Some(&contract))
                    .unwrap()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for network in &networks {
            assert_eq!(network.config, config);
            assert!(network.restore_policy_barrier);
            assert_eq!(network.attachment, networks[0].attachment);
        }
        assert_eq!(networks[0].policy, config.deny_all_policy().unwrap());
        assert_eq!(networks[1].policy, networks[0].policy);
        assert_eq!(microvm_ingress(&networks[0].policy), None);
        assert_eq!(
            networks[2].policy.host_loopback_action(),
            EgressAction::Deny
        );
        assert!(networks[2].policy.proxy_endpoint().is_some());
        assert_eq!(
            microvm_ingress(&networks[3].policy),
            Some(openvmm_defs::microvm::MicrovmIngress::PrivateNetworks)
        );
        assert_ne!(networks[2].policy, networks[3].policy);
        assert_eq!(
            contract.microvm_network.unwrap().egress_policy_mode,
            "deny-all"
        );
    }

    #[test]
    fn platform_template_rejects_address_family_cidr_and_revision_mismatches() {
        let config: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        let contract = network_contract_with_topology(&config, true);
        for address in ["10.0.0.3/24", "10.0.0.2/25", "10.1.0.2/24"] {
            let options = template_options(&["--network-profile", "portable", "--net", address]);
            let error = effective_microvm_network(&options, Some(&contract))
                .err()
                .unwrap();
            assert!(error.to_string().contains("address identity"), "{error}");
        }
        let options = template_options(&["--network-profile", "portable", "--net", "10.0.0.2/24"]);
        let ipv4_only = network_contract_with_topology(&config.without_ipv6(), true);
        assert!(effective_microvm_network(&options, Some(&ipv4_only)).is_err());
        let mut unknown = contract;
        unknown
            .microvm_network
            .as_mut()
            .unwrap()
            .egress_policy_encoding_version = 0;
        assert!(effective_microvm_network(&options, Some(&unknown)).is_err());
        unknown
            .microvm_network
            .as_mut()
            .unwrap()
            .egress_policy_encoding_version = 4;
        unknown.platform_template_version += 1;
        assert!(effective_microvm_network(&options, Some(&unknown)).is_err());
    }

    #[test]
    fn template_policy_never_relaxes_workload_or_no_nic_snapshots() {
        let options = template_options(&[
            "--network-profile",
            "portable",
            "--network-egress",
            "allow",
            "--host-loopback",
            "allow",
        ]);
        assert!(effective_microvm_network(&options, Some(&network_contract())).is_err());
        let mut no_nic = network_contract();
        no_nic.microvm_network = None;
        no_nic
            .devices
            .retain(|device| device.stable_id != MICROVM_NETWORK_STABLE_ID);
        no_nic
            .attachments
            .retain(|attachment| attachment.stable_id != MICROVM_NETWORK_STABLE_ID);
        assert!(effective_microvm_network(&options, Some(&no_nic)).is_err());
        let no_network = template_options(&[]);
        assert!(
            effective_microvm_network(&no_network, Some(&no_nic))
                .unwrap()
                .is_none()
        );
        let cold = Options::try_parse_from(["openvmm", "--machine", "microvm"]).unwrap();
        assert!(effective_microvm_network(&cold, None).unwrap().is_none());
    }

    #[test]
    fn template_requires_explicit_network_authorization_and_closed_capture() {
        let config = "10.0.0.2/24".parse().unwrap();
        let mut contract = network_contract_with_topology(&config, true);
        let unauthorized = template_options(&["--network-egress", "allow"]);
        assert!(effective_microvm_network(&unauthorized, Some(&contract)).is_err());
        contract
            .microvm_network
            .as_mut()
            .unwrap()
            .egress_policy_mode = "allow-all".to_owned();
        assert!(effective_microvm_network(&template_options(&[]), Some(&contract)).is_err());
    }

    #[test]
    fn network_restore_rebinds_endpoint_next_hops_and_requires_policy() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        let restored = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.config.guest_ipv4,
            std::net::Ipv4Addr::new(10, 0, 0, 2)
        );
        assert_eq!(
            restored.policy.next_hops(),
            &[
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                std::net::Ipv4Addr::new(10, 0, 0, 9),
            ]
        );
        let restored_again = effective_microvm_network(&options, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored_again.policy, restored.policy);

        let missing_policy = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
        ])
        .unwrap();
        assert!(effective_microvm_network(&missing_policy, Some(&contract)).is_err());
    }

    #[test]
    fn network_restore_keeps_the_ipv6_identity_that_the_snapshot_records() {
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        let dual_stack = effective_microvm_network(&options, Some(&network_contract()))
            .unwrap()
            .unwrap();
        let ipv6 = dual_stack.config.ipv6.unwrap();
        assert_eq!(
            ipv6.guest_ipv6,
            "fd00::a00:2".parse::<std::net::Ipv6Addr>().unwrap()
        );
        assert_eq!(
            dual_stack.policy.ipv6_link().map(|link| link.guest()),
            Some(ipv6.guest_ipv6)
        );

        // A snapshot whose network predates IPv6 restores without IPv6.
        let ipv4_only = "10.0.0.2/24"
            .parse::<openvmm_defs::microvm::MicrovmNetworkConfig>()
            .unwrap()
            .without_ipv6();
        let restored = effective_microvm_network(&options, Some(&network_contract_for(&ipv4_only)))
            .unwrap()
            .unwrap();
        assert_eq!(restored.config, ipv4_only);
        assert!(restored.policy.ipv6_link().is_none());
        assert_eq!(restored.policy.encoding_version(), 3);
    }

    #[test]
    fn legacy_network_policy_contract_alignment_preserves_validated_digest() {
        let mut saved = network_contract();
        let saved_network = saved.microvm_network.as_mut().unwrap();
        saved_network.egress_policy_encoding_version = 0;
        saved_network.egress_policy_sha256 = vec![0x5a; 32];
        let mut expected = network_contract();

        align_legacy_network_policy_contract(&saved, &mut expected);
        assert_eq!(saved.microvm_network, expected.microvm_network);
    }

    #[test]
    fn network_restore_rejects_attachment_identity_change() {
        let mut contract = network_contract();
        let attachment = contract
            .attachments
            .iter_mut()
            .find(|attachment| attachment.stable_id == MICROVM_NETWORK_STABLE_ID)
            .unwrap();
        attachment.identity.push(b'x');
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--network-profile",
            "portable",
            "--allow-endpoint",
            "192.0.2.7:443",
            "--allow-endpoint",
            "10.0.0.9:443",
            "--allow-endpoint",
            "10.0.0.9:8443",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    #[test]
    fn network_restore_requires_portable_profile() {
        let contract = network_contract();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--allow-host",
            "192.0.2.0/24",
        ])
        .unwrap();
        assert!(effective_microvm_network(&options, Some(&contract)).is_err());
    }

    #[test]
    fn microvm_ingress_follows_the_ingress_and_host_loopback_actions() {
        use openvmm_defs::microvm::MicrovmIngress;

        let network: openvmm_defs::microvm::MicrovmNetworkConfig = "10.0.0.2/24".parse().unwrap();
        for (arguments, expected) in [
            (
                &["--network-ingress", "allow", "--host-loopback", "allow"][..],
                Some(MicrovmIngress::PrivateNetworks),
            ),
            (
                &["--network-ingress", "allow"][..],
                Some(MicrovmIngress::PrivateNetworks),
            ),
            (
                &["--host-loopback", "allow", "--network-egress", "deny"][..],
                Some(MicrovmIngress::HostLoopback),
            ),
            (&["--host-loopback", "deny"][..], None),
            (&[][..], None),
        ] {
            let options = Options::try_parse_from(
                [
                    "openvmm",
                    "--machine",
                    "microvm",
                    "--network-profile",
                    "portable",
                    "--net",
                    "10.0.0.2/24",
                ]
                .into_iter()
                .chain(arguments.iter().copied()),
            )
            .unwrap();
            options.validate_microvm_options().unwrap();
            let policy = options.microvm_egress_policy(&network).unwrap();
            assert_eq!(microvm_ingress(&policy), expected, "{arguments:?}");
        }
    }
}
