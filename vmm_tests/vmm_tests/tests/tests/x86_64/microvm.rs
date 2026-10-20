// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM lifecycle integration tests.

use anyhow::Context;
use mesh::CancelContext;
use pal_async::DefaultDriver;
use pal_async::process::PolledChild;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_vmm_test::artifacts;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use vmm_test_macros::openvmm_test_no_agent;

fn portb_output(bytes: &[u8]) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let mut output = String::new();
    for byte in bytes {
        writeln!(
            output,
            "printf '\\x{byte:02x}' | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null"
        )?;
    }
    Ok(output)
}

#[openvmm_test_no_agent(linux_direct_x64)]
async fn phase_1_lifecycle(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    const BOOT_MARKER: &[u8] = b"OPENVMM-LINUX-DIRECT-TEST-READY\n";
    const RAW_MARKER: &[u8] = b"\0\r\n\x7f\xffLINUX-DIRECT-ECHO\n";
    const COMMAND_PING: u8 = 1;
    const COMMAND_ECHO: u8 = 2;
    const COMMAND_SNAPSHOT: u8 = 3;
    const COMMAND_STATE: u8 = 5;
    const COMMAND_SHUTDOWN: u8 = 6;

    let script = format!(
        "#!/bin/busybox sh\n\
         mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n\
         read_port() {{\n\
             dd if=/dev/port bs=1 skip=\"$1\" count=1 2>/dev/null | od -An -tu1 | tr -d ' '\n\
         }}\n\
         next_byte() {{\n\
             while :; do\n\
                 status=$(read_port 234)\n\
                 [ -n \"$status\" ] || status=0\n\
                 if [ $((status & 1)) -ne 0 ]; then\n\
                     read_port 233\n\
                     return\n\
                 fi\n\
             done\n\
         }}\n\
         generation=0\n\
         {}\
         while :; do\n\
             command=$(next_byte)\n\
             case \"$command\" in\n\
                 {COMMAND_PING})\n\
                     {}\
                     ;;\n\
                 {COMMAND_ECHO})\n\
                     {}\
                     ;;\n\
                 {COMMAND_SNAPSHOT})\n\
                     {}\
                     printf '\\x00' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null\n\
                     generation=$((generation + 1))\n\
                     {}\
                     ;;\n\
                 {COMMAND_STATE})\n\
                     if [ \"$generation\" = 1 ]; then\n\
                         {}\
                     else\n\
                         {}\
                     fi\n\
                     ;;\n\
                 {COMMAND_SHUTDOWN})\n\
                     status=$(next_byte)\n\
                     octal=$(printf '%03o' \"$status\")\n\
                     printf \"\\\\$octal\" | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n\
                     while :; do sleep 1; done\n\
                     ;;\n\
                 *)\n\
                     {}\
                     ;;\n\
             esac\n\
         done\n",
        portb_output(BOOT_MARKER)?,
        portb_output(b"PONG\n")?,
        portb_output(RAW_MARKER)?,
        portb_output(b"SNAPSHOT-REQUESTED\n")?,
        portb_output(b"SNAPSHOT-CONTINUED=1\n")?,
        portb_output(b"STATE=1\n")?,
        portb_output(b"STATE-INVALID\n")?,
        portb_output(b"UNKNOWN-COMMAND\n")?,
    );
    let config = config.with_microvm_machine(1);
    let initrd = config.prepare_initrd_with_file("microvm-test", script.as_bytes(), 0o100755)?;
    let mut vm = config
        .with_prebuilt_initrd(initrd)
        .modify_backend(|config| {
            config.with_linux_command_line(|cmdline| {
                assert!(cmdline.contains("rdinit=/microvm-test"));
            })
        })
        .run_without_agent()
        .await?;

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(BOOT_MARKER))
        .await
        .context("timed out waiting for microVM Linux direct boot marker")??;

    let save_error = vm
        .backend()
        .save_state()
        .await
        .expect_err("microVM host save unexpectedly succeeded");
    assert!(
        format!("{save_error:#}").contains("save is unavailable for microVM"),
        "unexpected microVM host-save error: {save_error:#}"
    );
    let pulse_error = vm
        .backend()
        .pulse_save_restore()
        .await
        .expect_err("microVM pulse save/restore unexpectedly succeeded");
    assert!(
        format!("{pulse_error:#}")
            .contains("save and restore are unavailable for this machine profile"),
        "unexpected microVM pulse-save error: {pulse_error:#}"
    );

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_PING])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("PONG"))
        .await
        .context("microVM Linux direct guest did not answer ping")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_ECHO])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(RAW_MARKER))
        .await
        .context("microVM raw binary echo was not observed")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SNAPSHOT])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(
            vm.backend()
                .wait_for_microvm_portb_output("SNAPSHOT-CONTINUED=1"),
        )
        .await
        .context("microVM snapshot request without a destination did not continue")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_STATE])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("STATE=1"))
        .await
        .context("microVM guest state did not persist after the snapshot request")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SHUTDOWN, 37])
        .await?;

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM status shutdown")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "expected microVM shutdown status 37, got {}",
        halt.detail
    );

    vm.teardown().await
}

petri::test!(
    filesystem_topology_linux_direct_cold_inventory,
    |resolver| {
        if petri_artifacts_common::tags::MachineArch::host()
            != petri_artifacts_common::tags::MachineArch::X86_64
        {
            return None;
        }
        Some([
            resolver.require(artifacts::OPENVMM_NATIVE).erase(),
            resolver
                .require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64)
                .erase(),
            resolver
                .require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64)
                .erase(),
        ])
    }
);

async fn filesystem_topology_linux_direct_cold_inventory(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel, initrd]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    const MEMORY_BYTES: u64 = 128 * 1024 * 1024;
    let base_initrd = std::fs::read(initrd.get())?;
    for (name, has_fs, mapped, capture) in [
        ("default", true, false, false),
        ("no-device-v1", false, false, false),
        ("mapped", true, true, false),
        ("platform-capture", true, false, true),
    ] {
        let directory = tempfile::tempdir()?;
        let distro = directory.path().join("distro.raw");
        let scratch = directory.path().join("scratch.raw");
        for path in [&distro, &scratch] {
            std::fs::File::create(path)?.set_len(1024 * 1024)?;
        }
        let share = directory.path().join("share");
        std::fs::create_dir(&share)?;
        let snapshot = directory.path().join("snapshot");
        let script = filesystem_inventory_script(has_fs, mapped, capture);
        let boot_initrd =
            initrd_cpio::inject_into_initrd(&base_initrd, "init", script.as_bytes(), 0o100755)?;
        let initrd_path = directory.path().join("initrd.cpio.gz");
        std::fs::write(&initrd_path, boot_initrd)?;

        let mut command = Command::new(&openvmm);
        command
            .args([
                "--machine",
                "microvm",
                "--memory",
                "128M",
                "--single-process",
            ])
            .arg("--kernel")
            .arg(kernel.get())
            .arg("--initrd")
            .arg(&initrd_path)
            .arg("--microvm-sandbox-block")
            .arg(format!("distro:file:{},ro", distro.display()))
            .arg("--microvm-sandbox-block")
            .arg(format!("scratch:file:{}", scratch.display()));
        if !has_fs {
            command.args(["--microvm-filesystem-topology", "no-device-v1"]);
        }
        if mapped {
            command
                .args(["--mount-aggregate", "/shares", "--mount-child"])
                .arg(format!("one,{},ro", share.display()));
        }
        if capture {
            command
                .arg("--snapshot-destination")
                .arg(&snapshot)
                .args(["--snapshot-tier", "platform"]);
        }
        let (status, output) =
            run_filesystem_topology_process(&driver, &params, &mut command, &format!("{name}.log"))
                .await?;
        anyhow::ensure!(status.success(), "{name} exited with {status}");
        anyhow::ensure!(
            output.contains("FILESYSTEM-TOPOLOGY-VERIFIED"),
            "{name} did not verify its real guest device inventory"
        );
        anyhow::ensure!(
            !output.contains("FILESYSTEM-TOPOLOGY-FAILED"),
            "{name} rejected its guest device inventory"
        );
        if capture {
            let (manifest, _) =
                openvmm_helpers::snapshot::restore::read_snapshot(&snapshot, MEMORY_BYTES)?;
            let contract = manifest
                .machine_contract
                .as_ref()
                .context("template capture has no machine contract")?;
            let filesystem = contract
                .devices
                .iter()
                .find(|device| device.stable_id == "fs:microvm0")
                .context("template capture lost its dormant filesystem device")?;
            assert_eq!(filesystem.irq, Some(6));
            assert_eq!(
                filesystem.state_unit_name,
                format!("virtiofs-{}", 0xd000_1000_u64)
            );
            assert!(filesystem.ranges.iter().any(|range| {
                range.address_space == "mmio"
                    && range.start == 0xd000_1000
                    && range.length == 0x1000
            }));
            assert!(contract.microvm_filesystem.is_none());
            assert_eq!(
                contract.microvm_filesystem_slot_version,
                openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION
            );
        } else {
            anyhow::ensure!(
                !snapshot.exists(),
                "{name} unexpectedly captured a snapshot"
            );
        }
    }
    Ok(())
}

fn filesystem_inventory_script(has_fs: bool, mapped: bool, capture: bool) -> String {
    let expected_fs = u8::from(has_fs);
    let expected_exports = if mapped { 4 } else { 0 };
    // The packaged kernel has no command-line virtio-mmio driver discovery.
    // Probe the actual VMM registers from the real Linux guest instead.
    format!(
        "#!/bin/busybox sh\n\
         portb() {{\n\
             for byte in $(printf '%s\\n' \"$*\" | od -An -tu1); do\n\
                 octal=$(printf '%03o' \"$byte\")\n\
                 printf \"\\\\$octal\" | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null\n\
             done\n\
         }}\n\
         fail() {{\n\
             portb \"FILESYSTEM-TOPOLOGY-FAILED $*\"\n\
             printf '\\x5d' | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n\
             while :; do sleep 1; done\n\
         }}\n\
         mount -t devtmpfs devtmpfs /dev || fail devtmpfs\n\
         mount -t proc proc /proc || fail procfs\n\
         mount -t sysfs sysfs /sys || fail sysfs\n\
         read_mmio() {{\n\
             value=$(/bin/busybox devmem \"$1\" 32 2>&1) || fail \"mmio-read $1 $value\"\n\
             value=${{value#0x}}\n\
             [ \"${{#value}}\" = 8 ] || fail \"mmio-read $1 $value\"\n\
             printf '%s' \"$value\" | tr A-F a-f\n\
         }}\n\
         devices=0; filesystems=0; blocks=0; networks=0\n\
         for base in 0xd0000000 0xd0001000 0xd0002000 0xd0003000 0xd0004000 0xd0005000 0xd0006000 0xd0007000; do\n\
             magic=$(read_mmio \"$base\")\n\
             [ \"$magic\" = 74726976 ] || continue\n\
             devices=$((devices + 1))\n\
             id=$(read_mmio \"$((base + 8))\")\n\
             portb \"VIRTIO-MMIO base=$base device=$id\"\n\
             case \"$id\" in\n\
                 0000001a) filesystems=$((filesystems + 1)); [ \"$base\" = 0xd0001000 ] || fail filesystem-slot;;\n\
                 00000002) blocks=$((blocks + 1));\n\
                     case \"$base\" in 0xd0003000|0xd0006000) ;; *) fail block-slot;; esac;;\n\
                 00000001) networks=$((networks + 1));;\n\
                 *) fail unexpected-device;;\n\
             esac\n\
         done\n\
         portb \"VIRTIO-INVENTORY devices=$devices fs=$filesystems blocks=$blocks net=$networks\"\n\
         [ \"$filesystems\" = {expected_fs} ] || fail filesystem-count\n\
         [ \"$blocks\" = 2 ] || fail block-count\n\
         [ \"$networks\" = 0 ] || fail network-count\n\
         [ \"$devices\" = $((2 + {expected_fs})) ] || fail device-count\n\
         discovery=0; exports=0; distro=0; scratch=0\n\
         for token in $(cat /proc/cmdline); do\n\
             case \"$token\" in\n\
                 virtio_mmio.device=0x1000@0xd0001000:6) discovery=$((discovery + 1));;\n\
                 virtio_mmio.device=0x1000@0xd0003000:4) distro=$((distro + 1));;\n\
                 virtio_mmio.device=0x1000@0xd0006000:11) scratch=$((scratch + 1));;\n\
                 virtio_mmio.device=*) fail unexpected-discovery;;\n\
                 virtfs_*) exports=$((exports + 1));;\n\
             esac\n\
         done\n\
         [ \"$discovery\" = {expected_fs} ] || fail filesystem-discovery\n\
         [ \"$distro\" = 1 ] && [ \"$scratch\" = 1 ] || fail block-discovery\n\
         [ \"$exports\" = {expected_exports} ] || fail filesystem-bootstrap\n\
         magic=$(read_mmio 0xd0001000)\n\
         portb \"FILESYSTEM-MMIO-MAGIC=$magic\"\n\
         if [ {expected_fs} = 1 ]; then\n\
             [ \"$magic\" = 74726976 ] || fail filesystem-mmio-magic\n\
             [ \"$(read_mmio 0xd0001100)\" = 7263696d ] || fail filesystem-tag-prefix\n\
             [ \"$(read_mmio 0xd0001104)\" = 006d766f ] || fail filesystem-tag-suffix\n\
             portb \"FILESYSTEM-TAG=microvm mmio=0xd0001000 irq=6\"\n\
         else\n\
             [ \"$magic\" != 74726976 ] || fail unexpected-filesystem-mmio\n\
         fi\n\
         portb \"FILESYSTEM-TOPOLOGY-VERIFIED discovery=$discovery exports=$exports\"\n\
         printf '\\x00' | dd of=/dev/port bs=1 seek={} count=1 conv=notrunc 2>/dev/null\n\
         {}\
         while :; do sleep 1; done\n",
        if capture { 1541 } else { 1540 },
        if capture {
            "fail snapshot-returned\n"
        } else {
            ""
        },
    )
}

petri::test!(filesystem_topology_invalid_launches, |resolver| {
    Some(resolver.require(artifacts::OPENVMM_NATIVE).erase())
});

async fn filesystem_topology_invalid_launches(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    openvmm: ResolvedArtifact,
) -> anyhow::Result<()> {
    for (index, extra) in [
        vec!["--mount", "/share,missing,ro"],
        vec![
            "--mount-aggregate",
            "/shares",
            "--mount-child",
            "one,missing,ro",
        ],
        vec!["--mount-deny", "missing"],
        vec![
            "--snapshot-destination",
            "missing",
            "--snapshot-tier",
            "platform",
        ],
        vec!["--restore-snapshot", "missing"],
        vec!["--rpc", "path=missing,transport=ttrpc"],
    ]
    .into_iter()
    .enumerate()
    {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("missing");
        std::fs::write(&marker, b"must survive rejected launch")?;
        let mut command = Command::new(&openvmm);
        command.current_dir(directory.path()).args([
            "--machine",
            "microvm",
            "--microvm-filesystem-topology",
            "no-device-v1",
            "--microvm-sandbox-block",
            "distro:mem:1M,ro",
            "--microvm-sandbox-block",
            "scratch:mem:1M",
        ]);
        if !extra.contains(&"--restore-snapshot") {
            command.args(["--kernel", "missing"]);
        }
        command.args(extra);
        let (status, output) = run_filesystem_topology_process(
            &driver,
            &params,
            &mut command,
            &format!("invalid-{index}.log"),
        )
        .await?;
        anyhow::ensure!(!status.success(), "invalid topology unexpectedly launched");
        anyhow::ensure!(
            output.contains("--microvm-filesystem-topology no-device-v1"),
            "topology was not rejected before opening host resources: {output}"
        );
        anyhow::ensure!(
            std::fs::read(&marker)? == b"must survive rejected launch",
            "rejected topology modified a host resource"
        );
    }
    Ok(())
}

struct FilesystemTopologyChild(PolledChild<std::process::Child>);

impl Drop for FilesystemTopologyChild {
    fn drop(&mut self) {
        let child = self.0.get_mut();
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if let Err(error) = child.kill() {
            tracing::error!(%error, "failed to kill the filesystem topology test VM");
            return;
        }
        if let Err(error) = child.wait() {
            tracing::error!(%error, "failed to reap the filesystem topology test VM");
        }
    }
}

async fn run_filesystem_topology_process(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    command: &mut Command,
    log_name: &str,
) -> anyhow::Result<(std::process::ExitStatus, String)> {
    let log: std::fs::File = params.log_source.create_attachment(log_name)?.into();
    let child = command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("failed to launch the filesystem topology test VM")?;
    let mut child =
        FilesystemTopologyChild(PolledChild::<std::process::Child>::new(driver, child)?);
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(child.0.wait())
        .await
        .with_context(|| format!("{log_name}: filesystem topology test VM timed out"))??;
    let output = std::fs::read_to_string(params.log_source.output_dir().join(log_name))?;
    Ok((status, output))
}
