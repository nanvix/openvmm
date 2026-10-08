// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::windows::OpenWindowsPipeSerialConfig;
use crate::windows::WindowsPipeSerialBackend;
use futures::future::poll_fn;
use pal::windows::pipe::Disposition;
use pal::windows::pipe::PipeExt;
use pal::windows::pipe::PipeMode;
use pal::windows::pipe::new_named_pipe;
use pal_async::DefaultDriver;
use pal_async_test::async_test;
use serial_core::SerialIo;
use std::fs::OpenOptions;
use windows_sys::Win32::Foundation::GENERIC_READ;
use windows_sys::Win32::Foundation::GENERIC_WRITE;
use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;

#[async_test]
async fn reconnects_after_client_close(driver: DefaultDriver) {
    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();
    let mut backend = WindowsPipeSerialBackend::new(
        Box::new(driver.clone()),
        OpenWindowsPipeSerialConfig::from(server),
    )
    .unwrap();

    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    client.set_pipe_mode(PIPE_NOWAIT).unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
    drop(client);
    poll_fn(|cx| backend.poll_disconnect(cx)).await.unwrap();
    backend.disconnect_current().unwrap();

    let _replacement = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
}

/// Connects to the pipe that `SERIAL_SOCKET_TEST_PIPE` names and exits.
#[test]
fn exiting_client_child() {
    let Some(path) = std::env::var_os("SERIAL_SOCKET_TEST_PIPE") else {
        return;
    };
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
}

#[async_test]
async fn accepts_the_next_client_after_an_unresolved_identity(driver: DefaultDriver) {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    // Without a console, no console host keeps the exited client open.
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let mut id = [0; 16];
    getrandom::fill(&mut id).unwrap();
    let path = format!(r#"\\.\pipe\{:0x}"#, u128::from_ne_bytes(id));
    let server = new_named_pipe(
        &path,
        GENERIC_READ | GENERIC_WRITE,
        Disposition::Create,
        PipeMode::Byte,
    )
    .unwrap();
    let mut backend = WindowsPipeSerialBackend::new(
        Box::new(driver.clone()),
        OpenWindowsPipeSerialConfig::from(server),
    )
    .unwrap();

    // The client connects and exits before the backend resolves its identity.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "windows_tests::exiting_client_child"])
        .env("SERIAL_SOCKET_TEST_PIPE", &path)
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let process_id = child.id();
    assert!(child.wait().unwrap().success());
    drop(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while pal::windows::security::user_sid::process_user_sid(process_id).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the exited client's identity stayed resolvable"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap_err();
    assert!(!backend.is_connected());

    // The backend rejected that client and listens for the next one.
    let _next = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    poll_fn(|cx| backend.poll_connect(cx)).await.unwrap();
    assert!(backend.local_peer_identity().unwrap().is_some());
}
