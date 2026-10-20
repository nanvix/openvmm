// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! New-process fixed-NIC template policy and pre-vCPU barrier evidence.

use super::portb_output;
use crate::ttrpc::OpenvmmChild;
use anyhow::Context as _;
use futures::AsyncReadExt as _;
use mesh::CancelContext;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn as _;
use pal_async::task::Task;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

const KERNEL_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_KERNEL";
const INITRD_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_INITRD";

petri::multitest!({
    let test = petri::SimpleTest::new_async(
        "fixed_nic_template_policy_barrier",
        |resolver: &petri::ArtifactResolver<'_>| {
            Some([
                resolver.require(artifacts::OPENVMM_NATIVE).erase(),
                resolver
                    .require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64)
                    .erase(),
                resolver
                    .require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64)
                    .erase(),
            ])
        },
        fixed_nic_template_policy_barrier,
    );
    // The packaged kernel lacks CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES.
    let test = if std::env::var_os(KERNEL_OVERRIDE).is_some()
        || std::env::var_os(INITRD_OVERRIDE).is_some()
    {
        test
    } else {
        test.ignore()
    };
    vec![test.into()]
});

struct NetworkVm {
    child: OpenvmmChild,
    stdin: std::process::ChildStdin,
    stdout: PolledPipe,
    output: Vec<u8>,
    _stderr: Task<anyhow::Result<()>>,
}

impl NetworkVm {
    fn launch(
        driver: &DefaultDriver,
        params: &petri::PetriTestParams<'_>,
        name: &str,
        command: &mut Command,
    ) -> anyhow::Result<Self> {
        let (stdout, stdout_write) = pal::pipe_pair()?;
        let (stderr, stderr_write) = pal::pipe_pair()?;
        let mut child = command
            .arg("--single-process")
            .stdin(Stdio::piped())
            .stdout(stdout_write)
            .stderr(stderr_write)
            .spawn()?;
        let stdin = child.stdin.take().context("missing test VM stdin")?;
        let child = OpenvmmChild(PolledChild::new(driver, child)?);
        let stderr = driver.spawn(
            "network-vm-stderr",
            petri::log_task(
                params.log_source.log_file(&format!("{name}-stderr"))?,
                PolledPipe::new(driver, stderr)?,
                "fixed-NIC test VM",
            ),
        );
        Ok(Self {
            child,
            stdin,
            stdout: PolledPipe::new(driver, stdout)?,
            output: Vec::new(),
            _stderr: stderr,
        })
    }

    async fn wait_for(&mut self, marker: &[u8]) -> anyhow::Result<()> {
        CancelContext::new()
            .with_timeout(Duration::from_secs(60))
            .until_cancelled(async {
                let mut buffer = [0; 4096];
                loop {
                    let count = self.stdout.read(&mut buffer).await?;
                    anyhow::ensure!(count != 0, "test VM closed stdout before its marker");
                    self.output.extend_from_slice(&buffer[..count]);
                    if self
                        .output
                        .split_inclusive(|byte| *byte == b'\n')
                        .any(|line| line.starts_with(b"FIXED-NIC-FAILED:") && line.ends_with(b"\n"))
                    {
                        anyhow::bail!(
                            "guest network probe failed: {}",
                            String::from_utf8_lossy(&self.output)
                        );
                    }
                    if self
                        .output
                        .windows(marker.len())
                        .any(|bytes| bytes == marker)
                    {
                        return Ok(());
                    }
                }
            })
            .await
            .with_context(|| {
                format!(
                    "test VM did not reach {:?}; output: {}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&self.output),
                )
            })?
    }

    async fn exited(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        CancelContext::new()
            .with_timeout(Duration::from_secs(30))
            .until_cancelled(self.child.wait())
            .await
            .context("test VM did not exit")?
            .context("failed to wait for test VM")
    }
}

fn no_connections(listeners: &[TcpListener; 2]) -> anyhow::Result<()> {
    for listener in listeners {
        match listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("failed to inspect policy probe listener"),
            Ok(_) => anyhow::bail!("a packet crossed the held or deny-all policy barrier"),
        }
    }
    Ok(())
}

async fn fixed_nic_template_policy_barrier(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel, initrd]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    let kernel_override = std::env::var_os(KERNEL_OVERRIDE).map(std::path::PathBuf::from);
    let initrd_override = std::env::var_os(INITRD_OVERRIDE).map(std::path::PathBuf::from);
    anyhow::ensure!(
        kernel_override.is_some() == initrd_override.is_some(),
        "{KERNEL_OVERRIDE} and {INITRD_OVERRIDE} must be set together"
    );
    let kernel_path = kernel_override.as_deref().unwrap_or_else(|| kernel.get());
    let base_initrd = initrd_override.as_deref().unwrap_or_else(|| initrd.get());
    let directory = tempfile::tempdir()?;
    let snapshot = directory.path().join("template");
    let listeners = [
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?,
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?,
    ];
    for listener in &listeners {
        listener.set_nonblocking(true)?;
    }
    let ports = [
        listeners[0].local_addr()?.port(),
        listeners[1].local_addr()?.port(),
    ];
    let script = format!(
        r#"#!/bin/busybox sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
mount -t devtmpfs devtmpfs /dev || exit 1
mount -t proc proc /proc || exit 1
mount -t sysfs sysfs /sys || exit 1
read_port() {{
    dd if=/dev/port bs=1 skip="$1" count=1 2>/dev/null | od -An -tu1 | tr -d ' '
}}
write_port() {{
    octal=$(printf '%03o' "$2")
    printf "\\$octal" | dd of=/dev/port bs=1 seek="$1" count=1 conv=notrunc 2>/dev/null
}}
say() {{
    for byte in $(printf '%s\n' "$*" | od -An -tu1); do
        write_port 233 "$byte"
    done
}}
fail() {{
    say "FIXED-NIC-FAILED:$*"
    write_port 1540 1
    while :; do sleep 1; done
}}
unconfigured() {{
    [ -e /sys/class/net/eth0/address ] || fail missing-nic
    [ "$(cat /sys/class/net/eth0/address)" = 52:54:00:00:00:02 ] || fail mac
    addresses=$(ip -4 -o addr show dev eth0) || fail ipv4-query
    [ -z "$addresses" ] || fail "configured-ipv4:$addresses"
    addresses=$(ip -6 -o addr show dev eth0) || fail ipv6-query
    [ -z "$addresses" ] || fail "configured-ipv6:$addresses"
}}
unconfigured
{}
write_port 1541 0
unconfigured
write_port 234 165
magic="$(read_port 233) $(read_port 233) $(read_port 233) $(read_port 233)"
[ "$magic" = "79 86 82 4" ] || fail
read_port 233 >/dev/null
read_port 233 >/dev/null
ranges=$(read_port 233)
remaining=$((25 + ranges * 16 + 64))
while [ "$remaining" -gt 0 ]; do
    read_port 233 >/dev/null
    remaining=$((remaining - 1))
done
write_port 1541 2
while [ $(( $(read_port 234) & 1 )) -eq 0 ]; do sleep 0.01; done
command=$(read_port 233)
if [ "$command" = 49 ]; then
    ip link set eth0 up || fail
    ip addr add 10.0.0.2/24 dev eth0 || fail
    ip route add default via 10.0.0.1 || fail
    printf POLICY-A | nc -w 1 10.0.0.1 {} >/dev/null 2>&1
    printf POLICY-B | nc -w 1 10.0.0.1 {} >/dev/null 2>&1
else
    [ "$command" = 48 ] || fail
    unconfigured
fi
{}
write_port 1540 0
while :; do sleep 1; done
"#,
        portb_output(b"FIXED-NIC-CAPTURE\n")?,
        ports[0],
        ports[1],
        portb_output(b"FIXED-NIC-PROBES-DONE\n")?,
    );
    let initrd_path = directory.path().join("network-initrd.cpio.gz");
    std::fs::write(
        &initrd_path,
        initrd_cpio::inject_into_initrd(
            &std::fs::read(base_initrd)?,
            "init",
            script.as_bytes(),
            0o100755,
        )?,
    )?;
    let mut capture = Command::new(&openvmm);
    capture
        .args(["--machine", "microvm", "--memory", "size=128MB"])
        .arg("--kernel")
        .arg(kernel_path)
        .arg("--initrd")
        .arg(&initrd_path)
        .arg("--snapshot-destination")
        .arg(&snapshot)
        .args([
            "--snapshot-tier",
            "platform",
            "--platform-template",
            "--net",
            "10.0.0.2/24",
            "--network-profile",
            "portable",
        ]);
    let mut source = NetworkVm::launch(&driver, &params, "capture", &mut capture)?;
    source.wait_for(b"FIXED-NIC-CAPTURE\n").await?;
    anyhow::ensure!(source.exited().await?.success(), "template capture failed");
    no_connections(&listeners)?;
    let (manifest, _) =
        openvmm_helpers::snapshot::restore::read_snapshot(&snapshot, 128 * 1024 * 1024)?;
    let contract = manifest
        .machine_contract
        .context("missing template machine contract")?;
    anyhow::ensure!(
        contract.platform_template_version == 1,
        "missing template revision"
    );
    anyhow::ensure!(
        !contract.effective_command_line.contains("virtnet_"),
        "template configured its guest NIC or announcer"
    );

    let restore_command = |arguments: &[String]| {
        let mut command = Command::new(&openvmm);
        command
            .args(["--machine", "microvm", "--restore-snapshot"])
            .arg(&snapshot)
            .args(arguments);
        command
    };
    for index in 0..4 {
        let mut arguments = Vec::new();
        if index < 3 {
            arguments.extend(["--network-profile".to_owned(), "portable".to_owned()]);
        }
        match index {
            0 | 1 => arguments.extend([
                "--network-proxy".to_owned(),
                format!("10.0.0.1:{}", ports[index]),
            ]),
            2 => arguments.extend(["--network-egress".to_owned(), "deny".to_owned()]),
            _ => {}
        }
        let forward_port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        if index < 2 {
            arguments.extend([
                "--host-loopback".to_owned(),
                "allow".to_owned(),
                "--host-loopback-forward".to_owned(),
                format!("tcp:{forward_port}:8080"),
            ]);
        }
        // This is a distinct paused restore of the same immutable template.
        // No command is submitted to the REPL or its history.
        let mut paused_command = restore_command(&arguments);
        paused_command.arg("--paused");
        let mut held = NetworkVm::launch(
            &driver,
            &params,
            &format!("held-{index}"),
            &mut paused_command,
        )?;
        held.stdin.write_all(&[0x11])?;
        held.wait_for(b"openvmm> ").await?;
        anyhow::ensure!(
            TcpStream::connect_timeout(
                &(Ipv4Addr::LOCALHOST, forward_port).into(),
                Duration::from_millis(250),
            )
            .is_err(),
            "the VMM admitted an ingress packet before policy installation"
        );
        no_connections(&listeners)?;
        drop(held);

        let mut command = restore_command(&arguments);
        let mut restored =
            NetworkVm::launch(&driver, &params, &format!("restored-{index}"), &mut command)?;
        restored
            .stdin
            .write_all(if index == 3 { b"0" } else { b"1" })?;
        restored.wait_for(b"FIXED-NIC-PROBES-DONE\n").await?;
        anyhow::ensure!(
            restored.exited().await?.success(),
            "restored network probe failed"
        );
        if index < 2 {
            let (mut connection, _) = listeners[index]
                .accept()
                .context("authorized guest packet did not reach its host endpoint")?;
            let mut bytes = [0; 8];
            std::io::Read::read_exact(&mut connection, &mut bytes)?;
            anyhow::ensure!(
                &bytes == if index == 0 { b"POLICY-A" } else { b"POLICY-B" },
                "guest packet reached the wrong policy endpoint"
            );
        }
        no_connections(&listeners)?;
    }

    for (index, address) in ["10.0.0.3/24", "10.0.0.2/25"].into_iter().enumerate() {
        let mut command = restore_command(&[
            "--network-profile".to_owned(),
            "portable".to_owned(),
            "--net".to_owned(),
            address.to_owned(),
        ]);
        let mut rejected = NetworkVm::launch(
            &driver,
            &params,
            &format!("address-mismatch-{index}"),
            &mut command,
        )?;
        anyhow::ensure!(
            !rejected.exited().await?.success(),
            "address mismatch restored"
        );
    }
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let mut command = restore_command(&[
        "--network-profile".to_owned(),
        "portable".to_owned(),
        "--host-loopback".to_owned(),
        "allow".to_owned(),
        "--host-loopback-forward".to_owned(),
        format!("tcp:{}:8080", occupied.local_addr()?.port()),
    ]);
    let mut rejected = NetworkVm::launch(&driver, &params, "bind-failure", &mut command)?;
    anyhow::ensure!(
        !rejected.exited().await?.success(),
        "listener bind failure restored"
    );
    let mut command = restore_command(&[
        "--network-profile".to_owned(),
        "portable".to_owned(),
        "--host-loopback".to_owned(),
        "allow".to_owned(),
        "--network-proxy".to_owned(),
        format!("10.0.0.1:{}", ports[0]),
        "--host-loopback-forward".to_owned(),
        format!("tcp:{}:8080", ports[0]),
    ]);
    let mut rejected = NetworkVm::launch(&driver, &params, "proxy-conflict", &mut command)?;
    anyhow::ensure!(
        !rejected.exited().await?.success(),
        "proxy/forward conflict restored"
    );
    no_connections(&listeners)?;
    anyhow::ensure!(snapshot.is_dir(), "restore removed its template");
    Ok(())
}
