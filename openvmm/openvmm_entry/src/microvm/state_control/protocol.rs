// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wire format of the microVM state-control protocol, version 1.
//!
//! Every message starts with a 24-byte little-endian header. Requests carry no
//! payload except the authentication capability. Responses carry a 32-byte
//! state record and an optional UTF-8 detail.

/// ASCII magic of every message.
pub(crate) const MAGIC: [u8; 4] = *b"NVXV";
/// The protocol version.
pub(crate) const VERSION: u16 = 1;
/// The header length.
pub(crate) const HEADER_LEN: usize = 24;
/// The length of the authentication capability.
pub(crate) const CAPABILITY_LEN: usize = 32;
/// The length of the state record.
pub(crate) const STATE_RECORD_LEN: usize = 32;
/// The longest detail a response carries.
pub(crate) const MAX_DETAIL_LEN: usize = 512;
/// The bit that marks a response operation.
pub(crate) const RESPONSE: u8 = 0x80;

/// A request operation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Operation {
    /// Present the capability; must be the first request.
    Authenticate = 1,
    /// Report the run state.
    Query = 2,
    /// Pause the VM and hold guest time.
    Pause = 3,
    /// Resume a VM the host paused.
    Resume = 4,
}

impl Operation {
    fn from_wire(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Authenticate,
            2 => Self::Query,
            3 => Self::Pause,
            4 => Self::Resume,
            _ => return None,
        })
    }

    /// The payload length a request of this operation carries.
    fn request_payload_len(self) -> usize {
        match self {
            Self::Authenticate => CAPABILITY_LEN,
            Self::Query | Self::Pause | Self::Resume => 0,
        }
    }
}

/// The status of a response.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    /// The operation completed; the record is the resulting state.
    Ok = 0,
    /// A snapshot boundary or post-restore gate is active. Nothing changed;
    /// retry later.
    Busy = 1,
    /// The operation was refused. The VM is in the recorded state.
    Rejected = 2,
    /// The operation failed and the VM state may be uncertain.
    Failed = 3,
}

/// The run state carried by a state record.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WireState {
    /// The VM worker did not answer.
    Unknown = 0,
    /// The guest is running.
    Running = 1,
    /// The host paused the guest and holds its time.
    Paused = 2,
    /// The VM is stopped for another reason.
    Stopped = 3,
    /// A snapshot boundary or post-restore gate is active.
    Busy = 4,
}

/// A malformed request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProtocolError {
    #[error("invalid state-control magic")]
    Magic,
    #[error("unsupported state-control protocol version {0}")]
    Version(u16),
    #[error("unknown state-control operation {0:#x}")]
    Operation(u8),
    #[error("state-control flags must be zero")]
    Flags,
    #[error("state-control request status must be zero")]
    RequestStatus,
    #[error("invalid state-control payload length {0}")]
    Length(u32),
}

/// A decoded request header.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct RequestHeader {
    /// The operation.
    pub operation: Operation,
    /// The client-chosen sequence the response echoes.
    pub sequence: u64,
}

impl RequestHeader {
    /// The payload length that follows the header.
    pub fn payload_len(&self) -> usize {
        self.operation.request_payload_len()
    }
}

/// The state record of a response.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct StateRecord {
    /// The run state.
    pub state: WireState,
    /// How many times the VM entered or left the host-paused state.
    pub transitions: u64,
    /// The control-console broker instance ID of this VMM process.
    pub instance_id: [u8; 16],
}

impl StateRecord {
    fn encode(&self) -> [u8; STATE_RECORD_LEN] {
        let mut bytes = [0; STATE_RECORD_LEN];
        bytes[0] = self.state as u8;
        bytes[8..16].copy_from_slice(&self.transitions.to_le_bytes());
        bytes[16..32].copy_from_slice(&self.instance_id);
        bytes
    }
}

fn header(kind: u8, sequence: u64, status: u32, length: usize) -> [u8; HEADER_LEN] {
    let mut bytes = [0; HEADER_LEN];
    bytes[0..4].copy_from_slice(&MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
    bytes[6] = kind;
    bytes[8..16].copy_from_slice(&sequence.to_le_bytes());
    bytes[16..20].copy_from_slice(&status.to_le_bytes());
    bytes[20..24].copy_from_slice(&(length as u32).to_le_bytes());
    bytes
}

/// Decodes and validates a request header.
pub(crate) fn decode_request_header(
    bytes: &[u8; HEADER_LEN],
) -> Result<RequestHeader, ProtocolError> {
    if bytes[0..4] != MAGIC {
        return Err(ProtocolError::Magic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(ProtocolError::Version(version));
    }
    let operation = Operation::from_wire(bytes[6]).ok_or(ProtocolError::Operation(bytes[6]))?;
    if bytes[7] != 0 {
        return Err(ProtocolError::Flags);
    }
    if bytes[16..20] != [0; 4] {
        return Err(ProtocolError::RequestStatus);
    }
    let length = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    if length as usize != operation.request_payload_len() {
        return Err(ProtocolError::Length(length));
    }
    Ok(RequestHeader {
        operation,
        sequence: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
    })
}

/// Truncates `detail` to at most [`MAX_DETAIL_LEN`] bytes on a character
/// boundary.
fn bounded_detail(detail: &str) -> &str {
    if detail.len() <= MAX_DETAIL_LEN {
        return detail;
    }
    let mut end = MAX_DETAIL_LEN;
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    &detail[..end]
}

/// Encodes a response.
pub(crate) fn encode_response(
    operation: Operation,
    sequence: u64,
    status: Status,
    record: &StateRecord,
    detail: &str,
) -> Vec<u8> {
    let detail = bounded_detail(detail).as_bytes();
    let mut bytes = header(
        operation as u8 | RESPONSE,
        sequence,
        status as u32,
        STATE_RECORD_LEN + detail.len(),
    )
    .to_vec();
    bytes.extend_from_slice(&record.encode());
    bytes.extend_from_slice(detail);
    bytes
}

/// A reference client codec for tests.
#[cfg(test)]
pub(crate) mod client {
    use super::*;

    /// A decoded response.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Response {
        pub operation: Operation,
        pub sequence: u64,
        pub status: Status,
        pub record: StateRecord,
        pub detail: String,
    }

    /// Encodes a request. `payload` is the capability of
    /// [`Operation::Authenticate`] and empty otherwise.
    pub(crate) fn encode_request(operation: Operation, sequence: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = header(operation as u8, sequence, 0, payload.len()).to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    /// Decodes consecutive responses, failing on any malformed one.
    pub(crate) fn decode_responses(mut bytes: &[u8]) -> Result<Vec<Response>, String> {
        let mut responses = Vec::new();
        while !bytes.is_empty() {
            let header = bytes.get(..HEADER_LEN).ok_or("truncated header")?;
            if header[0..4] != MAGIC || header[4..6] != VERSION.to_le_bytes() || header[7] != 0 {
                return Err("invalid header".into());
            }
            let kind = header[6];
            if kind & RESPONSE == 0 {
                return Err(format!("not a response: {kind:#x}"));
            }
            let operation = Operation::from_wire(kind & !RESPONSE).ok_or("unknown operation")?;
            let status = match u32::from_le_bytes(header[16..20].try_into().unwrap()) {
                0 => Status::Ok,
                1 => Status::Busy,
                2 => Status::Rejected,
                3 => Status::Failed,
                status => return Err(format!("unknown status {status}")),
            };
            let length = u32::from_le_bytes(header[20..24].try_into().unwrap()) as usize;
            if !(STATE_RECORD_LEN..=STATE_RECORD_LEN + MAX_DETAIL_LEN).contains(&length) {
                return Err(format!("invalid length {length}"));
            }
            let payload = bytes
                .get(HEADER_LEN..HEADER_LEN + length)
                .ok_or("truncated payload")?;
            let state = match payload[0] {
                0 => WireState::Unknown,
                1 => WireState::Running,
                2 => WireState::Paused,
                3 => WireState::Stopped,
                4 => WireState::Busy,
                state => return Err(format!("unknown state {state}")),
            };
            if payload[1..8] != [0; 7] {
                return Err("nonzero reserved bytes".into());
            }
            responses.push(Response {
                operation,
                sequence: u64::from_le_bytes(header[8..16].try_into().unwrap()),
                status,
                record: StateRecord {
                    state,
                    transitions: u64::from_le_bytes(payload[8..16].try_into().unwrap()),
                    instance_id: payload[16..32].try_into().unwrap(),
                },
                detail: String::from_utf8(payload[STATE_RECORD_LEN..].to_vec())
                    .map_err(|_| "detail is not UTF-8")?,
            });
            bytes = &bytes[HEADER_LEN + length..];
        }
        Ok(responses)
    }
}

#[cfg(test)]
mod tests {
    use super::client::*;
    use super::*;

    const INSTANCE: [u8; 16] = [7; 16];

    fn record(state: WireState, transitions: u64) -> StateRecord {
        StateRecord {
            state,
            transitions,
            instance_id: INSTANCE,
        }
    }

    #[test]
    fn request_header_has_the_documented_layout() {
        let bytes = encode_request(Operation::Pause, 0x0102_0304_0506_0708, &[]);
        assert_eq!(
            bytes,
            [
                b'N', b'V', b'X', b'V', 1, 0, 3, 0, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        let header = decode_request_header(bytes.as_slice().try_into().unwrap()).unwrap();
        assert_eq!(
            header,
            RequestHeader {
                operation: Operation::Pause,
                sequence: 0x0102_0304_0506_0708,
            }
        );
        assert_eq!(header.payload_len(), 0);

        let auth = encode_request(Operation::Authenticate, 1, &[0x5a; CAPABILITY_LEN]);
        assert_eq!(auth.len(), HEADER_LEN + CAPABILITY_LEN);
        assert_eq!(&auth[20..24], &32u32.to_le_bytes());
        let header = decode_request_header(auth[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(header.payload_len(), CAPABILITY_LEN);
    }

    #[test]
    fn malformed_request_headers_are_rejected() {
        let valid = encode_request(Operation::Query, 9, &[]);
        let decode = |mutate: &dyn Fn(&mut Vec<u8>)| {
            let mut bytes = valid.clone();
            mutate(&mut bytes);
            decode_request_header(bytes.as_slice().try_into().unwrap())
        };
        assert_eq!(decode(&|b| b[0] = b'X'), Err(ProtocolError::Magic));
        assert_eq!(decode(&|b| b[4] = 2), Err(ProtocolError::Version(2)));
        assert_eq!(decode(&|b| b[6] = 0), Err(ProtocolError::Operation(0)));
        assert_eq!(decode(&|b| b[6] = 5), Err(ProtocolError::Operation(5)));
        assert_eq!(
            decode(&|b| b[6] = 2 | RESPONSE),
            Err(ProtocolError::Operation(0x82))
        );
        assert_eq!(decode(&|b| b[7] = 1), Err(ProtocolError::Flags));
        assert_eq!(decode(&|b| b[16] = 1), Err(ProtocolError::RequestStatus));
        assert_eq!(decode(&|b| b[20] = 1), Err(ProtocolError::Length(1)));
        // Authenticate must carry exactly the capability.
        assert_eq!(
            decode(&|b| b[6] = Operation::Authenticate as u8),
            Err(ProtocolError::Length(0))
        );
    }

    #[test]
    fn responses_have_the_documented_layout_and_bounded_detail() {
        let bytes = encode_response(
            Operation::Resume,
            42,
            Status::Ok,
            &record(WireState::Running, 2),
            "",
        );
        assert_eq!(bytes.len(), HEADER_LEN + STATE_RECORD_LEN);
        assert_eq!(bytes[6], Operation::Resume as u8 | RESPONSE);
        assert_eq!(
            &bytes[HEADER_LEN..],
            &[
                1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
                7, 7, 7, 7
            ]
        );
        assert_eq!(
            decode_responses(&bytes).unwrap(),
            [Response {
                operation: Operation::Resume,
                sequence: 42,
                status: Status::Ok,
                record: record(WireState::Running, 2),
                detail: String::new(),
            }]
        );

        let long = "é".repeat(MAX_DETAIL_LEN);
        let bytes = encode_response(
            Operation::Pause,
            1,
            Status::Rejected,
            &record(WireState::Running, 0),
            &long,
        );
        let response = decode_responses(&bytes).unwrap().remove(0);
        assert_eq!(response.status, Status::Rejected);
        assert_eq!(response.detail.len(), MAX_DETAIL_LEN);
        assert!(long.starts_with(&response.detail));
    }
}
