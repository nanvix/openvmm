// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::cleanup_socket;
use anyhow::Context;
use pal_async::driver::Driver;
#[cfg(windows)]
use pal_async::pipe::PolledPipe;
use serial_socket::net::OpenSocketSerialConfig;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Read;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::Path;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

#[cfg(unix)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(
    Resource<SerialBackendHandle>,
    pal_async::socket::PolledSocket<unix_socket::UnixStream>,
)> {
    let (left, right) = unix_socket::UnixStream::pair()?;
    let right = pal_async::socket::PolledSocket::new(driver, right)?;
    Ok((OpenSocketSerialConfig::from(left).into_resource(), right))
}

#[cfg(windows)]
pub fn anonymous_serial_pair(
    driver: &(impl Driver + ?Sized),
) -> io::Result<(Resource<SerialBackendHandle>, PolledPipe)> {
    use serial_socket::windows::OpenWindowsPipeSerialConfig;

    // Use named pipes on Windows even though we also support Unix sockets
    // there. This avoids an unnecessary winsock dependency.
    let (server, client) = pal::windows::pipe::bidirectional_pair(false)?;
    let server = PolledPipe::new(driver, server)?;
    // Use the client for the VM side so that it does not try to reconnect
    // (which isn't possible via pal_async for pipes opened in non-overlapped
    // mode, anyway).
    Ok((
        OpenWindowsPipeSerialConfig::from(client).into_resource(),
        server,
    ))
}

pub fn bind_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    bind_serial_inner(path, true)
}

/// Binds a listener without removing an existing socket path.
pub fn bind_serial_without_cleanup(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    bind_serial_inner(path, false)
}

#[cfg(target_os = "linux")]
pub fn bind_control_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control endpoint has no parent directory",
        )
    })?;
    let parent_metadata = fs_err::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != pal::unix::effective_user_id()
        || parent_metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control endpoint parent must be a non-symlink directory owned by OpenVMM with mode 0700",
        ));
    }

    let listener = UnixListener::bind(path)?;
    let bound_metadata = fs_err::symlink_metadata(path)?;
    let prepare_result = (|| {
        fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let socket_metadata = fs_err::symlink_metadata(path)?;
        if !socket_metadata.file_type().is_socket()
            || socket_metadata.dev() != bound_metadata.dev()
            || socket_metadata.ino() != bound_metadata.ino()
            || socket_metadata.uid() != parent_metadata.uid()
            || socket_metadata.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control endpoint must be an owned Unix socket with mode 0600",
            ));
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        drop(listener);
        if let Ok(current) = fs_err::symlink_metadata(path)
            && current.file_type().is_socket()
            && current.dev() == bound_metadata.dev()
            && current.ino() == bound_metadata.ino()
        {
            let _ = fs_err::remove_file(path);
        }
        return Err(error);
    }
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}

#[cfg(not(target_os = "linux"))]
pub fn bind_control_serial(_path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure control-console local endpoints are only supported on Linux",
    ))
}

/// Consumes a one-way pipe containing exactly one nonzero control capability.
#[cfg(unix)]
pub fn read_control_capability(mut file: File) -> io::Result<[u8; 32]> {
    use std::os::unix::fs::FileTypeExt;

    if !file.metadata()?.file_type().is_fifo() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control authentication input is not a one-way pipe",
        ));
    }
    pal::unix::pipe::set_nonblocking(&file, true)?;

    let mut bytes = [0u8; 33];
    let mut count = 0;
    loop {
        match file.read(&mut bytes[count..]) {
            Ok(0) => break,
            Ok(read) => {
                count += read;
                if count == bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "control authentication payload has an invalid length",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control authentication writer was not closed",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    let capability = bytes[..count].try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "control authentication payload has an invalid length",
        )
    })?;
    if capability == [0; 32] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control authentication capability must not be zero",
        ));
    }
    Ok(capability)
}

/// Reads the prepared authentication pipe without taking ownership of descriptor 0.
#[cfg(target_os = "linux")]
pub fn read_control_capability_from_stdin() -> io::Result<[u8; 32]> {
    use std::os::fd::AsFd;

    let stdin = io::stdin();
    read_control_capability(File::from(stdin.as_fd().try_clone_to_owned()?))
}

fn bind_serial_inner(
    path: &Path,
    cleanup_existing: bool,
) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = pal::windows::pipe::new_named_pipe(
                path,
                windows_sys::Win32::Foundation::GENERIC_READ
                    | windows_sys::Win32::Foundation::GENERIC_WRITE,
                pal::windows::pipe::Disposition::Create,
                pal::windows::pipe::PipeMode::Byte,
            )?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    if cleanup_existing {
        cleanup_socket(path);
    }
    Ok(OpenSocketSerialConfig::from(UnixListener::bind(path)?).into_resource())
}

/// Connect to an existing named pipe or Unix domain socket as a client.
///
/// Unlike [`bind_serial`], which creates a new server, this function connects
/// to a pipe or socket that already exists.
pub fn connect_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    #[cfg(windows)]
    {
        use serial_socket::windows::OpenWindowsPipeSerialConfig;

        if path.starts_with("//./pipe") {
            let pipe = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?;
            return Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource());
        }
    }

    Ok(OpenSocketSerialConfig::from(unix_socket::UnixStream::connect(path)?).into_resource())
}

pub fn connect_serial_with_timeout(
    path: &Path,
    timeout: std::time::Duration,
) -> io::Result<Resource<SerialBackendHandle>> {
    let path = path.to_owned();
    let (send, recv) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("serial-connect".to_owned())
        .spawn(move || {
            let _ = send.send(connect_serial(&path));
        })?;
    match recv.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("serial endpoint did not connect within {timeout:?}"),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
            "serial connection worker terminated without a result",
        )),
    }
}

/// Connects a single-use restore-readiness event sink.
pub fn connect_restore_ready_sink(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;

        let socket = unix_socket::UnixStream::connect(path)?;
        Ok(File::from(OwnedFd::from(socket)))
    }

    #[cfg(windows)]
    {
        const NAMED_PIPE_PREFIX: &str = "//./pipe/";

        let normalized = path.to_string_lossy().replace('\\', "/");
        if !normalized.starts_with(NAMED_PIPE_PREFIX) || normalized.len() == NAMED_PIPE_PREFIX.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "restore readiness path must name a Windows //./pipe/... endpoint",
            ));
        }
        std::fs::OpenOptions::new().write(true).open(path)
    }
}

pub fn bind_tcp_serial(addr: &SocketAddr) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("failed to bind tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}

pub fn connect_tcp_serial(
    addr: &SocketAddr,
    timeout: std::time::Duration,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let stream = TcpStream::connect_timeout(addr, timeout)
        .with_context(|| format!("failed to connect to tcp address {addr}"))?;
    Ok(OpenSocketSerialConfig::from(stream).into_resource())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use test_with_tracing::test;

    fn read_capability_payload(payload: &[u8], keep_writer_open: bool) -> io::Result<[u8; 32]> {
        let (read, mut write) = pal::pipe_pair()?;
        write.write_all(payload)?;
        if !keep_writer_open {
            drop(write);
        }
        read_control_capability(read)
    }

    #[test]
    fn control_capability_requires_exact_closed_pipe_payload() {
        let mut capability = [0x5a; 32];
        capability[0] = 0;
        assert_eq!(
            read_capability_payload(&capability, false).unwrap(),
            capability
        );
        for length in [0, 1, 31] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        for length in [33, 64] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for length in [0, 31, 32] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], true)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert_eq!(
            read_capability_payload(&[0; 32], false).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn control_capability_rejects_non_pipe_input() {
        assert_eq!(
            read_control_capability(tempfile::tempfile().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn control_capability_stdin_child() {
        let Some(expected) = std::env::var_os("OPENVMM_TEST_CONTROL_STDIN") else {
            return;
        };
        let result = read_control_capability_from_stdin();
        match expected.to_str().unwrap() {
            "valid" => {
                let mut capability = [0x5a; 32];
                capability[0] = 0;
                assert_eq!(result.unwrap(), capability);
                assert_eq!(io::stdin().read(&mut [0]).unwrap(), 0);
            }
            "open-writer" => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut),
            "non-pipe" => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput),
            _ => panic!("unexpected child-test scenario"),
        }
    }

    #[test]
    fn control_capability_uses_prepared_child_stdin() {
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Duration;
        use std::time::Instant;

        for scenario in ["valid", "open-writer", "non-pipe"] {
            let (read, mut write) = pal::pipe_pair().unwrap();
            let mut capability = [0x5a; 32];
            capability[0] = 0;
            write.write_all(&capability).unwrap();
            let _writer = (scenario == "open-writer").then_some(write);
            let input = if scenario == "non-pipe" {
                tempfile::tempfile().unwrap()
            } else {
                read
            };
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "serial_io::tests::control_capability_stdin_child",
                    "--nocapture",
                ])
                .env("OPENVMM_TEST_CONTROL_STDIN", scenario)
                .stdin(Stdio::from(input))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("stdin capability scenario {scenario} timed out: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "stdin scenario {scenario}: {output:?}"
            );
        }
    }

    #[test]
    fn control_listener_has_private_permissions_and_exclusive_path() {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let directory = std::env::current_dir().unwrap().join(format!(
            ".control-endpoint-test-{:016x}",
            u64::from_ne_bytes(nonce)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let path = directory.join("control.sock");
        let cleanup_path = path.clone();
        let cleanup_directory = directory.clone();
        let _cleanup = pal::ScopeExit::new(move || {
            let _ = fs_err::remove_file(cleanup_path);
            let _ = fs_err::remove_dir(cleanup_directory);
        });

        let listener = bind_control_serial(&path).unwrap();
        let metadata = fs_err::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), pal::unix::effective_user_id());
        assert!(bind_control_serial(&path).is_err());
        drop(listener);
    }
}
