// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Authenticated host endpoint for microVM pause, resume, and run-state
//! queries.
//!
//! The endpoint is bound like the live control console: a private local
//! endpoint that serves one host at a time and checks the local peer identity
//! before the protocol starts. The first request must present the
//! control-console capability within the control-console authentication
//! timeout. A failed or stalled authentication, and any malformed request,
//! closes the connection without a response. The endpoint is served by the
//! VMM process outside the VM's device state units, so it answers while the
//! VM is paused.

mod protocol;

use self::protocol::Operation;
use self::protocol::StateRecord;
use self::protocol::Status;
use self::protocol::WireState;
use anyhow::Context as _;
use futures::AsyncRead;
use futures::AsyncReadExt as _;
use futures::AsyncWrite;
use futures::AsyncWriteExt as _;
use mesh::CancelContext;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend as _;
use openvmm_defs::rpc::MicrovmHostPauseError;
use openvmm_defs::rpc::MicrovmRunState;
use openvmm_defs::rpc::MicrovmRunStatus;
use openvmm_defs::rpc::VmRpc;
use pal_async::DefaultDriver;
use pal_async::task::Spawn as _;
use pal_async::task::Task;
use serial_core::LocalPeerIdentity;
use serial_core::SerialIo;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// How long an authenticated host may stay idle between requests.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait before accepting again after the endpoint failed to
/// accept a host.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The configuration of a state-control endpoint.
pub(crate) struct StateControlConfig {
    path: PathBuf,
    capability: [u8; protocol::CAPABILITY_LEN],
    instance_id: [u8; 16],
    expected_peer_identity: LocalPeerIdentity,
    auth_timeout: Duration,
}

impl StateControlConfig {
    /// Takes the capability, broker instance ID, expected peer identity, and
    /// authentication timeout of the live control console.
    pub(crate) fn new(
        path: PathBuf,
        broker: &virtio_resources::console::control::VirtioControlConsoleBrokerConfig,
    ) -> Self {
        Self {
            path,
            capability: broker.capability,
            instance_id: broker.instance_id,
            expected_peer_identity: broker.expected_peer_identity.clone(),
            auth_timeout: Duration::from_millis(broker.auth_timeout_ms),
        }
    }

    /// The endpoint path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// A bound state-control endpoint, ready to serve once the VM worker runs.
pub(crate) struct StateControlEndpoint {
    listener: Listener,
    driver: DefaultDriver,
    config: StateControlConfig,
}

#[cfg(target_os = "linux")]
type Listener = serial_socket::net::OpenSocketSerialConfig;
#[cfg(windows)]
type Listener = serial_socket::windows::OpenWindowsPipeSerialConfig;
#[cfg(not(any(target_os = "linux", windows)))]
type Listener = std::convert::Infallible;

impl StateControlEndpoint {
    /// Binds the endpoint exclusively, with the private permissions of the
    /// control console.
    pub(crate) fn bind(config: StateControlConfig, driver: &DefaultDriver) -> anyhow::Result<Self> {
        let listener = bind(&config.path).with_context(|| {
            format!(
                "failed to bind microVM state-control listener {}",
                config.path.display()
            )
        })?;
        Ok(Self {
            listener,
            driver: driver.clone(),
            config,
        })
    }

    /// Serves hosts until the returned task is dropped, controlling the VM
    /// worker behind `vm_rpc`.
    pub(crate) fn spawn(self, vm_rpc: mesh::Sender<VmRpc>) -> anyhow::Result<Task<()>> {
        let Self {
            listener,
            driver,
            config,
        } = self;
        let io = backend(listener, &driver)
            .context("failed to start the microVM state-control endpoint")?;
        Ok(driver.spawn("microvm-state-control", serve(io, config, vm_rpc)))
    }
}

#[cfg(any(target_os = "linux", windows))]
fn bind(path: &Path) -> std::io::Result<Listener> {
    crate::serial_io::microvm::bind_control_endpoint(path)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn bind(_path: &Path) -> std::io::Result<Listener> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "microVM state control requires Linux Unix sockets or Windows named pipes",
    ))
}

#[cfg(target_os = "linux")]
fn backend(listener: Listener, driver: &DefaultDriver) -> std::io::Result<Box<dyn SerialIo>> {
    Ok(Box::new(serial_socket::net::SocketSerialBackend::new(
        Box::new(driver.clone()),
        listener,
    )?))
}

#[cfg(windows)]
fn backend(listener: Listener, driver: &DefaultDriver) -> std::io::Result<Box<dyn SerialIo>> {
    Ok(Box::new(
        serial_socket::windows::WindowsPipeSerialBackend::new(Box::new(driver.clone()), listener)?,
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn backend(listener: Listener, _driver: &DefaultDriver) -> std::io::Result<Box<dyn SerialIo>> {
    match listener {}
}

/// Accepts hosts one at a time and serves each authenticated session.
async fn serve(mut io: Box<dyn SerialIo>, config: StateControlConfig, vm_rpc: mesh::Sender<VmRpc>) {
    loop {
        if let Err(error) = futures::future::poll_fn(|cx| io.poll_connect(cx)).await {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "microVM state-control endpoint failed to accept a host"
            );
            CancelContext::new()
                .with_timeout(ACCEPT_RETRY_DELAY)
                .cancelled()
                .await;
            continue;
        }
        match io.local_peer_identity() {
            Ok(Some(identity)) if identity == config.expected_peer_identity => {
                if let Err(error) = serve_session(&mut *io, &config, &vm_rpc).await {
                    tracelimit::warn_ratelimited!(
                        error = error.as_ref() as &dyn std::error::Error,
                        "microVM state-control session closed"
                    );
                }
            }
            identity => {
                tracelimit::warn_ratelimited!(
                    identity_available = matches!(identity, Ok(Some(_))),
                    "microVM state-control host rejected because its local peer identity is unavailable or unexpected"
                );
            }
        }
        if let Err(error) = io.disconnect_current() {
            tracing::error!(
                error = &error as &dyn std::error::Error,
                "microVM state-control endpoint cannot accept another host"
            );
            return;
        }
    }
}

/// Serves one connected host: authentication, then requests until the host
/// disconnects, stays idle for [`IDLE_TIMEOUT`], or sends a malformed request.
async fn serve_session(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin + ?Sized),
    config: &StateControlConfig,
    vm_rpc: &mesh::Sender<VmRpc>,
) -> anyhow::Result<()> {
    let (header, capability) = CancelContext::new()
        .with_timeout(config.auth_timeout)
        .until_cancelled(read_request(&mut *stream))
        .await
        .context("authentication timed out")??
        .context("host disconnected before authenticating")?;
    anyhow::ensure!(
        header.operation == Operation::Authenticate,
        "the first request did not authenticate"
    );
    anyhow::ensure!(
        capability_matches(&config.capability, &capability),
        "authentication failed"
    );
    let run = run_status(vm_rpc).await;
    respond(&mut *stream, header, Status::Ok, config, run, "").await?;

    loop {
        let Ok(request) = CancelContext::new()
            .with_timeout(IDLE_TIMEOUT)
            .until_cancelled(read_request(&mut *stream))
            .await
        else {
            return Ok(());
        };
        let Some((header, _)) = request? else {
            return Ok(());
        };
        let (status, run, detail) = match header.operation {
            Operation::Authenticate => anyhow::bail!("the host authenticated twice"),
            Operation::Query => (Status::Ok, run_status(vm_rpc).await, String::new()),
            Operation::Pause => transition(vm_rpc, VmRpc::MicrovmPause).await,
            Operation::Resume => transition(vm_rpc, VmRpc::MicrovmResume).await,
        };
        respond(&mut *stream, header, status, config, run, &detail).await?;
    }
}

/// Reads one request. Returns `None` if the host disconnected cleanly before
/// a header.
async fn read_request(
    stream: &mut (impl AsyncRead + Unpin + ?Sized),
) -> anyhow::Result<Option<(protocol::RequestHeader, [u8; protocol::CAPABILITY_LEN])>> {
    let mut header = [0; protocol::HEADER_LEN];
    let first = stream.read(&mut header).await?;
    if first == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut header[first..]).await?;
    let header = protocol::decode_request_header(&header)?;
    let mut capability = [0; protocol::CAPABILITY_LEN];
    if header.payload_len() != 0 {
        stream.read_exact(&mut capability).await?;
    }
    Ok(Some((header, capability)))
}

async fn respond(
    stream: &mut (impl AsyncWrite + Unpin + ?Sized),
    request: protocol::RequestHeader,
    status: Status,
    config: &StateControlConfig,
    run: Option<MicrovmRunStatus>,
    detail: &str,
) -> anyhow::Result<()> {
    let record = StateRecord {
        state: run.map_or(WireState::Unknown, |run| wire_state(run.state)),
        transitions: run.map_or(0, |run| run.transitions),
        instance_id: config.instance_id,
    };
    let response =
        protocol::encode_response(request.operation, request.sequence, status, &record, detail);
    stream.write_all(&response).await?;
    stream.flush().await?;
    Ok(())
}

fn wire_state(state: MicrovmRunState) -> WireState {
    match state {
        MicrovmRunState::Running => WireState::Running,
        MicrovmRunState::Paused => WireState::Paused,
        MicrovmRunState::Stopped => WireState::Stopped,
        MicrovmRunState::Busy => WireState::Busy,
    }
}

/// Returns the VM worker's run status, or `None` if it did not answer.
async fn run_status(vm_rpc: &mesh::Sender<VmRpc>) -> Option<MicrovmRunStatus> {
    vm_rpc.call(VmRpc::MicrovmRunState, ()).await.ok()
}

/// Asks the VM worker to pause or resume, then reports its run state.
async fn transition(
    vm_rpc: &mesh::Sender<VmRpc>,
    request: fn(Rpc<(), Result<bool, MicrovmHostPauseError>>) -> VmRpc,
) -> (Status, Option<MicrovmRunStatus>, String) {
    let (status, detail) = match vm_rpc.call(request, ()).await {
        Ok(Ok(_)) => (Status::Ok, String::new()),
        Ok(Err(MicrovmHostPauseError::Busy)) => (Status::Busy, String::new()),
        Ok(Err(error @ MicrovmHostPauseError::Rejected(_))) => {
            (Status::Rejected, error_detail(&error))
        }
        Ok(Err(error @ MicrovmHostPauseError::Uncertain(_))) => {
            (Status::Failed, error_detail(&error))
        }
        Err(error) => (Status::Failed, error_detail(&error)),
    };
    (status, run_status(vm_rpc).await, detail)
}

fn error_detail(error: &(dyn std::error::Error + 'static)) -> String {
    let mut detail = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        detail.push_str(": ");
        detail.push_str(&error.to_string());
        source = error.source();
    }
    detail
}

/// Compares the capability in constant time.
fn capability_matches(expected: &[u8; 32], actual: &[u8; 32]) -> bool {
    std::hint::black_box(
        expected
            .iter()
            .zip(actual)
            .fold(0, |difference, (expected, actual)| {
                difference | (expected ^ actual)
            }),
    ) == 0
}

#[cfg(test)]
mod tests {
    use super::protocol::client::Response;
    use super::protocol::client::decode_responses;
    use super::protocol::client::encode_request;
    use super::*;
    use futures::FutureExt as _;
    use mesh::error::RemoteError;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use test_with_tracing::test;

    const CAPABILITY: [u8; 32] = [0x5a; 32];
    const INSTANCE: [u8; 16] = [3; 16];

    fn config() -> StateControlConfig {
        StateControlConfig {
            path: PathBuf::from("state.sock"),
            capability: CAPABILITY,
            instance_id: INSTANCE,
            expected_peer_identity: LocalPeerIdentity::UnixUid(1),
            auth_timeout: Duration::from_secs(5),
        }
    }

    /// A host connection with scripted input that records the responses.
    struct Scripted {
        input: futures::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.input).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Scripted {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.output.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A VM worker that answers run-state, pause, and resume requests.
    #[derive(Default)]
    struct FakeWorker {
        state: Option<MicrovmRunState>,
        transitions: u64,
        pause_error: Option<fn() -> MicrovmHostPauseError>,
        resume_error: Option<fn() -> MicrovmHostPauseError>,
        requests: Vec<&'static str>,
    }

    impl FakeWorker {
        fn running() -> Self {
            Self {
                state: Some(MicrovmRunState::Running),
                ..Default::default()
            }
        }

        fn status(&self) -> MicrovmRunStatus {
            MicrovmRunStatus {
                state: self.state.unwrap(),
                transitions: self.transitions,
            }
        }

        async fn run(&mut self, mut recv: mesh::Receiver<VmRpc>) {
            while let Ok(message) = recv.recv().await {
                match message {
                    VmRpc::MicrovmRunState(rpc) => rpc.complete(self.status()),
                    VmRpc::MicrovmPause(rpc) => {
                        self.requests.push("pause");
                        let result = self.transition(
                            self.pause_error,
                            MicrovmRunState::Running,
                            MicrovmRunState::Paused,
                        );
                        rpc.complete(result);
                    }
                    VmRpc::MicrovmResume(rpc) => {
                        self.requests.push("resume");
                        let result = self.transition(
                            self.resume_error,
                            MicrovmRunState::Paused,
                            MicrovmRunState::Running,
                        );
                        rpc.complete(result);
                    }
                    message => panic!("unexpected VM RPC {message:?}"),
                }
            }
        }

        /// Moves from `from` to `to`; already being in `to` is idempotent.
        fn transition(
            &mut self,
            error: Option<fn() -> MicrovmHostPauseError>,
            from: MicrovmRunState,
            to: MicrovmRunState,
        ) -> Result<bool, MicrovmHostPauseError> {
            if let Some(error) = error {
                return Err(error());
            }
            match self.state.unwrap() {
                state if state == from => {
                    self.state = Some(to);
                    self.transitions += 1;
                    Ok(true)
                }
                state if state == to => Ok(false),
                state => Err(MicrovmHostPauseError::Rejected(RemoteError::new(
                    anyhow::anyhow!("cannot leave {state:?}"),
                ))),
            }
        }
    }

    fn requests(sequence: &[(Operation, &[u8])]) -> Vec<u8> {
        sequence
            .iter()
            .enumerate()
            .flat_map(|(index, (operation, payload))| {
                encode_request(*operation, index as u64 + 1, payload)
            })
            .collect()
    }

    /// Runs one scripted session against `worker` and returns the session
    /// result and responses.
    fn run_session(input: Vec<u8>, worker: &mut FakeWorker) -> (anyhow::Result<()>, Vec<Response>) {
        let (send, recv) = mesh::channel();
        let mut stream = Scripted {
            input: futures::io::Cursor::new(input),
            output: Vec::new(),
        };
        let config = config();
        let session = async {
            let result = serve_session(&mut stream, &config, &send).await;
            drop(send);
            result
        };
        let (result, ()) =
            futures::executor::block_on(futures::future::join(session, worker.run(recv)));
        (result, decode_responses(&stream.output).unwrap())
    }

    fn record(state: WireState, transitions: u64) -> StateRecord {
        StateRecord {
            state,
            transitions,
            instance_id: INSTANCE,
        }
    }

    #[test]
    fn pause_query_and_resume_report_observed_state() {
        let mut worker = FakeWorker::running();
        let (result, responses) = run_session(
            requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Pause, &[]),
                (Operation::Pause, &[]),
                (Operation::Query, &[]),
                (Operation::Resume, &[]),
                (Operation::Resume, &[]),
            ]),
            &mut worker,
        );
        result.unwrap();
        let expected = [
            (Operation::Authenticate, WireState::Running, 0),
            (Operation::Pause, WireState::Paused, 1),
            (Operation::Pause, WireState::Paused, 1),
            (Operation::Query, WireState::Paused, 1),
            (Operation::Resume, WireState::Running, 2),
            (Operation::Resume, WireState::Running, 2),
        ];
        assert_eq!(responses.len(), expected.len());
        for (sequence, (response, (operation, state, transitions))) in
            (1..).zip(responses.iter().zip(expected))
        {
            assert_eq!(response.operation, operation);
            assert_eq!(response.sequence, sequence);
            assert_eq!(response.status, Status::Ok);
            assert_eq!(response.record, record(state, transitions));
            assert!(response.detail.is_empty());
        }
        // Repeated pause and resume are idempotent in the worker.
        assert_eq!(worker.requests, ["pause", "pause", "resume", "resume"]);
    }

    #[test]
    fn transition_failures_map_to_their_statuses() {
        type Inject = fn() -> MicrovmHostPauseError;
        let busy: Inject = || MicrovmHostPauseError::Busy;
        let rejected: Inject =
            || MicrovmHostPauseError::Rejected(RemoteError::new(anyhow::anyhow!("periodic timer")));
        let uncertain: Inject =
            || MicrovmHostPauseError::Uncertain(RemoteError::new(anyhow::anyhow!("start failed")));
        for operation in [Operation::Pause, Operation::Resume] {
            for (error, status, state) in [
                (busy, Status::Busy, MicrovmRunState::Busy),
                (rejected, Status::Rejected, MicrovmRunState::Paused),
                (uncertain, Status::Failed, MicrovmRunState::Stopped),
            ] {
                let mut worker = FakeWorker {
                    state: Some(state),
                    pause_error: Some(error),
                    resume_error: Some(error),
                    ..Default::default()
                };
                let (result, responses) = run_session(
                    requests(&[(Operation::Authenticate, &CAPABILITY), (operation, &[])]),
                    &mut worker,
                );
                result.unwrap();
                assert_eq!(responses[1].operation, operation);
                assert_eq!(responses[1].status, status);
                assert_eq!(responses[1].record.state, wire_state(state));
                assert_eq!(responses[1].detail.is_empty(), status == Status::Busy);
            }
        }
    }

    #[test]
    fn resume_reports_the_state_the_worker_kept() {
        let mut worker = FakeWorker {
            state: Some(MicrovmRunState::Stopped),
            ..Default::default()
        };
        let (result, responses) = run_session(
            requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Resume, &[]),
            ]),
            &mut worker,
        );
        result.unwrap();
        assert_eq!(responses[1].status, Status::Rejected);
        assert_eq!(responses[1].record.state, WireState::Stopped);
        assert_eq!(worker.requests, ["resume"]);

        let mut worker = FakeWorker {
            state: Some(MicrovmRunState::Paused),
            transitions: 1,
            resume_error: Some(|| {
                MicrovmHostPauseError::Rejected(RemoteError::new(anyhow::anyhow!(
                    "TSC read-back mismatch"
                )))
            }),
            ..Default::default()
        };
        let (result, responses) = run_session(
            requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Resume, &[]),
            ]),
            &mut worker,
        );
        result.unwrap();
        // The held time is still in place, so the host can retry.
        assert_eq!(responses[1].status, Status::Rejected);
        assert_eq!(responses[1].record, record(WireState::Paused, 1));
        assert!(responses[1].detail.contains("TSC read-back mismatch"));
    }

    #[test]
    fn authentication_failures_close_without_a_response() {
        let mut wrong = CAPABILITY;
        wrong[31] ^= 1;
        for input in [
            requests(&[(Operation::Authenticate, &wrong)]),
            requests(&[(Operation::Query, &[])]),
            Vec::new(),
            // Truncated capability.
            requests(&[(Operation::Authenticate, &CAPABILITY)])[..40].to_vec(),
        ] {
            let mut worker = FakeWorker::running();
            let (result, responses) = run_session(input, &mut worker);
            assert!(result.is_err());
            assert!(responses.is_empty());
            assert!(worker.requests.is_empty());
        }
    }

    #[test]
    fn malformed_and_repeated_authentication_requests_close_the_session() {
        let mut bad = encode_request(Operation::Query, 2, &[]);
        bad[7] = 1;
        let mut input = requests(&[(Operation::Authenticate, &CAPABILITY)]);
        input.extend(bad);
        input.extend(encode_request(Operation::Pause, 3, &[]));
        let mut worker = FakeWorker::running();
        let (result, responses) = run_session(input, &mut worker);
        assert!(result.is_err());
        assert_eq!(responses.len(), 1);
        assert!(worker.requests.is_empty());

        let mut worker = FakeWorker::running();
        let (result, responses) = run_session(
            requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Authenticate, &CAPABILITY),
            ]),
            &mut worker,
        );
        assert!(result.is_err());
        assert_eq!(responses.len(), 1);
    }

    #[test]
    fn unavailable_worker_reports_unknown_state() {
        let (send, recv) = mesh::channel::<VmRpc>();
        drop(recv);
        let mut stream = Scripted {
            input: futures::io::Cursor::new(requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Pause, &[]),
                (Operation::Resume, &[]),
            ])),
            output: Vec::new(),
        };
        let config = config();
        serve_session(&mut stream, &config, &send)
            .now_or_never()
            .unwrap()
            .unwrap();
        let responses = decode_responses(&stream.output).unwrap();
        assert_eq!(responses[0].record.state, WireState::Unknown);
        assert_eq!(responses[1].status, Status::Failed);
        assert_eq!(responses[2].status, Status::Failed);
        assert_eq!(responses[2].record.state, WireState::Unknown);
    }

    #[test]
    fn capability_comparison_checks_every_byte() {
        assert!(capability_matches(&CAPABILITY, &CAPABILITY));
        for index in 0..32 {
            let mut other = CAPABILITY;
            other[index] ^= 0x80;
            assert!(!capability_matches(&CAPABILITY, &other));
        }
    }

    #[cfg(any(target_os = "linux", windows))]
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    #[cfg(target_os = "linux")]
    fn endpoint_path(directory: &tempfile::TempDir) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory.path().join("state.sock")
    }

    #[cfg(windows)]
    fn endpoint_path(_directory: &tempfile::TempDir) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\openvmm-state-control-test-{}",
            guid::Guid::new_random()
        ))
    }

    #[cfg(target_os = "linux")]
    fn current_peer_identity() -> LocalPeerIdentity {
        LocalPeerIdentity::UnixUid(pal::unix::effective_user_id())
    }

    #[cfg(windows)]
    fn current_peer_identity() -> LocalPeerIdentity {
        let sid = pal::windows::security::user_sid::current_process_user_sid().unwrap();
        let (bytes, length) = sid.to_fixed_bytes();
        LocalPeerIdentity::windows_sid(bytes, length).unwrap()
    }

    #[cfg(target_os = "linux")]
    fn connect(path: &Path) -> std::os::unix::net::UnixStream {
        std::os::unix::net::UnixStream::connect(path).unwrap()
    }

    #[cfg(windows)]
    fn connect(path: &Path) -> std::fs::File {
        let deadline = std::time::Instant::now() + TEST_TIMEOUT;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                // The endpoint listens again shortly after closing a host.
                Err(error)
                    if error.raw_os_error()
                        == Some(windows_sys::Win32::Foundation::ERROR_PIPE_BUSY as i32)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result.unwrap(),
            }
        }
    }

    /// Serves a real endpoint: a host with the wrong capability is closed
    /// without a response, and the next host is served.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn endpoint_closes_a_failed_host_and_serves_the_next() {
        use std::io::Read as _;
        use std::io::Write as _;

        let directory = tempfile::tempdir().unwrap();
        let path = endpoint_path(&directory);
        let config = StateControlConfig {
            path: path.clone(),
            expected_peer_identity: current_peer_identity(),
            ..config()
        };
        let (bound_send, bound_recv) = std::sync::mpsc::channel();
        let (done_send, done_recv) = mesh::oneshot::<()>();
        let server = std::thread::spawn(move || {
            pal_async::DefaultPool::run_with(async |driver| {
                let endpoint = StateControlEndpoint::bind(config, &driver).unwrap();
                bound_send.send(()).unwrap();
                let (send, recv) = mesh::channel();
                let _serving = endpoint.spawn(send).unwrap();
                let mut worker = FakeWorker::running();
                futures::future::select(std::pin::pin!(worker.run(recv)), done_recv).await;
                worker.requests
            })
        });
        bound_recv.recv_timeout(TEST_TIMEOUT).unwrap();

        let (host_send, host_recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut wrong = CAPABILITY;
            wrong[0] ^= 1;
            let mut host = connect(&path);
            host.write_all(&encode_request(Operation::Authenticate, 1, &wrong))
                .unwrap();
            let mut rejected = Vec::new();
            // Windows reports the closed pipe as an error rather than EOF.
            let _ = host.read_to_end(&mut rejected);
            drop(host);

            let mut host = connect(&path);
            host.write_all(&requests(&[
                (Operation::Authenticate, &CAPABILITY),
                (Operation::Pause, &[]),
            ]))
            .unwrap();
            let mut served = vec![0; 2 * (protocol::HEADER_LEN + protocol::STATE_RECORD_LEN)];
            host.read_exact(&mut served).unwrap();
            host_send
                .send((rejected, decode_responses(&served).unwrap()))
                .unwrap();
        });
        let (rejected, responses) = host_recv
            .recv_timeout(TEST_TIMEOUT)
            .expect("the endpoint did not serve the host");
        assert!(rejected.is_empty());
        assert_eq!(responses[0].status, Status::Ok);
        assert_eq!(responses[1].status, Status::Ok);
        assert_eq!(responses[1].record, record(WireState::Paused, 1));
        done_send.send(());
        assert_eq!(server.join().unwrap(), ["pause"]);
    }
}
