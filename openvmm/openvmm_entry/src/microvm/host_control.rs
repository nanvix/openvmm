// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Authenticated host-only control for the microVM profile.

use super::console::MicrovmControlAuthentication;
use anyhow::Context;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use mesh::CancelContext;
use mesh::rpc::RpcSend;
use pal_async::DefaultDriver;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_SIZE: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct MicrovmHostControlServer {
    io: Box<dyn serial_core::SerialIo>,
    authentication: MicrovmControlAuthentication,
    slots: Vec<Option<mesh::Sender<virtio_resources::blk::ImageSlotRequest>>>,
    transition: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct Request {
    version: u32,
    request_id: u64,
    #[serde(flatten)]
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Operation {
    QueryImageSlots,
    BindImageSlot {
        slot: u8,
        path: String,
        identity: String,
    },
}

#[derive(Serialize)]
struct Response {
    version: u32,
    request_id: u64,
    #[serde(flatten)]
    result: ResponseResult,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ResponseResult {
    Ok { result: ResponseBody },
    Error { code: &'static str, message: String },
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ResponseBody {
    QueryImageSlots { slots: Vec<SlotStatus> },
    BindImageSlot { slot: u8, capacity_sectors: u64 },
}

#[derive(Serialize)]
struct SlotStatus {
    slot: u8,
    name: String,
    state: &'static str,
    identity: Option<String>,
    capacity_sectors: u64,
}

struct TransitionGuard(Arc<AtomicBool>);

impl Drop for TransitionGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl MicrovmHostControlServer {
    pub(crate) fn bind(
        path: &Path,
        driver: &DefaultDriver,
        authentication: MicrovmControlAuthentication,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            io: crate::serial_io::microvm::bind_host_control(path, driver)
                .with_context(|| format!("failed to bind host control {}", path.display()))?,
            authentication,
            slots: Vec::new(),
            transition: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn set_image_slots(
        &mut self,
        slots: Vec<Option<mesh::Sender<virtio_resources::blk::ImageSlotRequest>>>,
        transition: Arc<AtomicBool>,
    ) {
        self.slots = slots;
        self.transition = transition;
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            futures::future::poll_fn(|cx| self.io.poll_connect(cx))
                .await
                .context("failed to accept a host-control connection")?;
            if let Err(error) = self.serve_connection().await {
                tracelimit::warn_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    "host-control connection failed"
                );
            }
            self.io
                .disconnect_current()
                .context("failed to reset the host-control listener")?;
        }
    }

    async fn serve_connection(&mut self) -> anyhow::Result<()> {
        let peer = self
            .io
            .local_peer_identity()
            .context("failed to identify the host-control peer")?
            .context("host-control endpoint did not authenticate its local peer")?;
        anyhow::ensure!(
            peer == self.authentication.expected_peer_identity,
            "host-control peer identity does not match the OpenVMM process owner"
        );

        let mut capability = [0; 32];
        CancelContext::new()
            .with_timeout(Duration::from_millis(self.authentication.auth_timeout_ms))
            .until_cancelled(self.io.read_exact(&mut capability))
            .await
            .context("host-control authentication timed out")?
            .context("failed to read host-control capability")?;
        anyhow::ensure!(
            capability_matches(&capability, &self.authentication.capability),
            "host-control capability is invalid"
        );

        loop {
            let Some(request) = self.read_request().await? else {
                return Ok(());
            };
            let response = self.handle_request(request).await;
            self.write_response(&response).await?;
        }
    }

    async fn read_request(&mut self) -> anyhow::Result<Option<Request>> {
        let Some(length) = CancelContext::new()
            .with_timeout(IDLE_TIMEOUT)
            .until_cancelled(read_frame_length(&mut self.io))
            .await
            .context("host-control connection was idle")?
            .context("failed to read host-control frame length")?
        else {
            return Ok(None);
        };
        let length = length as usize;
        anyhow::ensure!(
            (1..=MAX_FRAME_SIZE).contains(&length),
            "host-control frame length is invalid"
        );
        let payload = read_frame_payload(&mut self.io, length, IDLE_TIMEOUT).await?;
        serde_json::from_slice(&payload)
            .map(Some)
            .context("host-control request is invalid JSON")
    }

    async fn write_response(&mut self, response: &Response) -> anyhow::Result<()> {
        let payload = encode_response(response)?;
        let length = u32::try_from(payload.len()).context("host-control response is too large")?;
        self.io
            .write_all(&length.to_le_bytes())
            .await
            .context("failed to write host-control frame length")?;
        self.io
            .write_all(&payload)
            .await
            .context("failed to write host-control response")?;
        self.io
            .flush()
            .await
            .context("failed to flush host-control response")
    }

    async fn handle_request(&mut self, request: Request) -> Response {
        let result = if request.version != PROTOCOL_VERSION {
            Err((
                "unsupported_version",
                format!(
                    "host-control protocol version {} is unsupported",
                    request.version
                ),
            ))
        } else {
            match request.operation {
                Operation::QueryImageSlots => self.query_image_slots().await,
                Operation::BindImageSlot {
                    slot,
                    path,
                    identity,
                } => self.bind_image_slot(slot, Path::new(&path), identity).await,
            }
        };
        Response {
            version: PROTOCOL_VERSION,
            request_id: request.request_id,
            result: match result {
                Ok(result) => ResponseResult::Ok { result },
                Err((code, message)) => ResponseResult::Error { code, message },
            },
        }
    }

    async fn query_image_slots(&mut self) -> Result<ResponseBody, (&'static str, String)> {
        let mut slots = Vec::with_capacity(self.slots.len());
        for (index, requests) in self.slots.iter().enumerate() {
            let (state, identity, capacity_sectors) = if let Some(requests) = requests {
                let state = requests
                    .call(virtio_resources::blk::ImageSlotRequest::Query, ())
                    .await
                    .map_err(|error| ("device_unavailable", error.to_string()))?;
                (
                    if state.identity.is_some() {
                        "bound"
                    } else {
                        "empty"
                    },
                    state.identity,
                    state.capacity_sectors,
                )
            } else {
                ("inactive", None, 0)
            };
            slots.push(SlotStatus {
                slot: index as u8,
                name: openvmm_defs::microvm::microvm_image_slot_name(index as u8)
                    .map_err(|error| ("invalid_slot", error.to_string()))?,
                state,
                identity,
                capacity_sectors,
            });
        }
        Ok(ResponseBody::QueryImageSlots { slots })
    }

    async fn bind_image_slot(
        &mut self,
        slot: u8,
        path: &Path,
        identity: String,
    ) -> Result<ResponseBody, (&'static str, String)> {
        let transition = self.transition.clone();
        transition
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                (
                    "busy",
                    "image binding is excluded by a lifecycle transition".to_owned(),
                )
            })?;
        let _guard = TransitionGuard(transition);
        let requests = self
            .slots
            .get(slot as usize)
            .ok_or_else(|| ("invalid_slot", format!("image slot {slot} is out of range")))?
            .as_ref()
            .ok_or_else(|| ("inactive_slot", format!("image slot {slot} is inactive")))?;
        let current = requests
            .call(virtio_resources::blk::ImageSlotRequest::Query, ())
            .await
            .map_err(|error| ("device_unavailable", error.to_string()))?;
        if let Some(current_identity) = current.identity {
            return if current_identity == identity {
                Ok(ResponseBody::BindImageSlot {
                    slot,
                    capacity_sectors: current.capacity_sectors,
                })
            } else {
                Err((
                    "already_bound",
                    format!("image slot {slot} is already bound to a different identity"),
                ))
            };
        }
        let media =
            open_read_only_media(path).map_err(|error| ("invalid_media", format!("{error:#}")))?;
        let length = media
            .metadata()
            .map_err(|error| ("invalid_media", error.to_string()))?
            .len();
        let state = requests
            .call(
                virtio_resources::blk::ImageSlotRequest::Bind,
                virtio_resources::blk::BindImageSlotRequest {
                    media,
                    identity,
                    length,
                    logical_block_size: 512,
                    physical_block_size: 4096,
                },
            )
            .await
            .map_err(|error| ("device_unavailable", error.to_string()))?
            .map_err(|error| match error {
                virtio_resources::blk::BindImageSlotError::AlreadyBound => {
                    ("already_bound", error.to_string())
                }
                virtio_resources::blk::BindImageSlotError::GeometryMismatch => {
                    ("geometry_mismatch", error.to_string())
                }
                virtio_resources::blk::BindImageSlotError::InvalidMedia(_) => {
                    ("invalid_media", error.to_string())
                }
            })?;
        Ok(ResponseBody::BindImageSlot {
            slot,
            capacity_sectors: state.capacity_sectors,
        })
    }
}

fn capability_matches(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Reads a little-endian frame length. Returns `None` when the peer closed the connection
/// at a frame boundary, which is how a client ends a session.
async fn read_frame_length(
    io: &mut (impl futures::AsyncRead + Unpin),
) -> std::io::Result<Option<u32>> {
    let mut length = [0; 4];
    let mut filled = 0;
    while filled < length.len() {
        let count = match io.read(&mut length[filled..]).await {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if count == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            };
        }
        filled += count;
    }
    Ok(Some(u32::from_le_bytes(length)))
}

/// Reads a frame payload. A peer that stalls inside a frame is treated like an idle
/// peer, so it cannot hold the single-connection endpoint.
async fn read_frame_payload(
    io: &mut (impl futures::AsyncRead + Unpin),
    length: usize,
    timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let mut payload = vec![0; length];
    CancelContext::new()
        .with_timeout(timeout)
        .until_cancelled(io.read_exact(&mut payload))
        .await
        .context("host-control frame stalled")?
        .context("failed to read host-control frame")?;
    Ok(payload)
}

/// Encodes a response within the frame limit. Error messages can echo request
/// input, so a response that would exceed the limit becomes a bounded error.
fn encode_response(response: &Response) -> anyhow::Result<Vec<u8>> {
    let payload = serde_json::to_vec(response).context("failed to encode host-control response")?;
    if payload.len() <= MAX_FRAME_SIZE {
        return Ok(payload);
    }
    serde_json::to_vec(&Response {
        version: PROTOCOL_VERSION,
        request_id: response.request_id,
        result: ResponseResult::Error {
            code: "response_too_large",
            message: format!("host-control response exceeds the {MAX_FRAME_SIZE}-byte frame limit"),
        },
    })
    .context("failed to encode host-control response")
}

fn open_read_only_media(path: &Path) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open image media {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect image media {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "image media is not a regular file: {}",
        path.display()
    );
    anyhow::ensure!(
        metadata.len() != 0 && metadata.len().is_multiple_of(512),
        "image media length must be nonzero and 512-byte aligned"
    );
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::IDLE_TIMEOUT;
    use super::MAX_FRAME_SIZE;
    use super::PROTOCOL_VERSION;
    use super::Response;
    use super::ResponseResult;
    use super::encode_response;
    use super::read_frame_length;
    use super::read_frame_payload;
    use futures::executor::block_on;
    use futures::io::Cursor;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;

    fn error_response(request_id: u64, message: String) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            request_id,
            result: ResponseResult::Error {
                code: "invalid_media",
                message,
            },
        }
    }

    #[test]
    fn oversized_responses_become_bounded_errors() {
        let payload = encode_response(&error_response(7, "x".repeat(MAX_FRAME_SIZE))).unwrap();
        assert!(payload.len() <= MAX_FRAME_SIZE);
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["request_id"], 7);
        assert_eq!(value["status"], "error");
        assert_eq!(value["code"], "response_too_large");

        let payload = encode_response(&error_response(8, "short".to_owned())).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["request_id"], 8);
        assert_eq!(value["code"], "invalid_media");
        assert_eq!(value["message"], "short");
    }

    #[test]
    fn stalled_frame_payload_times_out() {
        struct Stalled;

        impl futures::AsyncRead for Stalled {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut [u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Pending
            }
        }

        let error = block_on(read_frame_payload(
            &mut Stalled,
            4,
            Duration::from_millis(10),
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("host-control frame stalled"));

        let mut io = Cursor::new(b"{}".to_vec());
        assert_eq!(
            block_on(read_frame_payload(&mut io, 2, IDLE_TIMEOUT)).unwrap(),
            b"{}"
        );
    }

    #[test]
    fn close_at_frame_boundary_ends_the_session() {
        let mut io = Cursor::new(Vec::new());
        assert_eq!(block_on(read_frame_length(&mut io)).unwrap(), None);
    }

    #[test]
    fn frame_length_is_little_endian() {
        let mut io = Cursor::new(vec![0x34, 0x12, 0, 0, 0xff]);
        assert_eq!(block_on(read_frame_length(&mut io)).unwrap(), Some(0x1234));
        assert_eq!(io.position(), 4);
    }

    #[test]
    fn close_inside_frame_length_is_an_error() {
        let mut io = Cursor::new(vec![1, 0]);
        assert_eq!(
            block_on(read_frame_length(&mut io)).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
}
