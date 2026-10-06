# MicroVM state-control protocol

`--microvm-state-control listen=<PATH>` gives the host a local endpoint to
pause, resume, and query a microVM that has a live authenticated control
console. OpenVMM serves the endpoint outside the VM's device state units, so it
answers while the VM is paused. The protocol is version 1.

## Endpoint and authentication

The endpoint is bound like the control console endpoint and must differ from
the boot console and control console endpoints. On Linux it is an AF_UNIX
socket with mode `0600` in an owned, non-symlink directory with mode `0700`,
and the peer UID must match the OpenVMM process. On Windows it is a
`//./pipe/...` named pipe with a protected DACL, and the token user SID of the
connecting process must match the OpenVMM process user.

The endpoint serves one host connection at a time:

1. OpenVMM checks the peer identity before reading from the connection.
2. The first request must be `AUTHENTICATE`, carrying the 32-byte
   control-console capability, within `--microvm-control-auth-timeout-ms`.
3. The host then sends requests. OpenVMM handles them one at a time and
   answers them in request order.

A failed or stalled authentication, a malformed request, or a second
`AUTHENTICATE` closes the connection without a response. OpenVMM also closes a
connection that sends no request for 60 seconds. The host may then reconnect.

The endpoint is never recorded in a snapshot. A restore caller may supply a
fresh endpoint with the restore command. When a snapshot commits, the source
removes its endpoint along with the control console endpoint.

## Header

Every request and response starts with a 24-byte little-endian header:

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 4 | ASCII magic `NVXV` |
| 4 | 2 | protocol version, `1` |
| 6 | 1 | operation; a response sets bit 7 (`0x80`) |
| 7 | 1 | flags, zero |
| 8 | 8 | sequence, chosen by the host and echoed in the response |
| 16 | 4 | status, zero in a request |
| 20 | 4 | payload length |

| Value | Operation | Request payload |
| ---: | --- | --- |
| 1 | `AUTHENTICATE` | exactly 32 capability bytes |
| 2 | `QUERY` | empty |
| 3 | `PAUSE` | empty |
| 4 | `RESUME` | empty |

A response payload is a 32-byte state record followed by an optional UTF-8
detail of at most 512 bytes. The detail explains a `REJECTED` or `FAILED`
status for logs; hosts must not parse it.

## State record

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 1 | run state |
| 1 | 7 | reserved, zero |
| 8 | 8 | transitions |
| 16 | 16 | VMM instance ID of the control-console broker |

| Value | Run state | Meaning |
| ---: | --- | --- |
| 0 | `UNKNOWN` | the VM worker did not answer |
| 1 | `RUNNING` | the guest is running |
| 2 | `PAUSED` | the host paused the VM and holds its guest time |
| 3 | `STOPPED` | the VM is stopped for another reason |
| 4 | `BUSY` | a snapshot boundary or post-restore gate is active |

`transitions` counts how many times the VM entered or left the host-paused
state since the VMM process started, so it is odd exactly while the host holds
a pause. A host can compare it across responses to detect a pause or resume it
did not make. The instance ID changes when a snapshot is restored into a new
VMM process.

## Status

| Value | Status | Meaning |
| ---: | --- | --- |
| 0 | `OK` | the operation completed; the record is the resulting state |
| 1 | `BUSY` | a snapshot boundary or post-restore gate is active; nothing changed, so retry later |
| 2 | `REJECTED` | the operation was refused; the VM keeps the recorded state |
| 3 | `FAILED` | the VM worker did not answer, or the VM could not start again; its state is uncertain and it must be torn down |

A successful `AUTHENTICATE` and every `QUERY` return `OK` with the current
state record.

## Pause and resume

`PAUSE` stops a running VM's vCPUs and devices and holds its guest time. Once
the vCPUs stop, OpenVMM takes VP 0's TSC and every vCPU's LAPIC state. Pausing
a VM that the host already paused returns `OK` and changes nothing. Pausing a
VM that is stopped for another reason is `REJECTED`.

A pause is `REJECTED` with state `RUNNING` when guest time cannot be held, for
example because a vCPU has an armed periodic or TSC-deadline LAPIC timer. This
is the same restriction as snapshot capture. The vCPUs start again, so the
guest only observes a brief stall. If the VM cannot start again, the status is
`FAILED`.

`RESUME` sets every vCPU's TSC back to the held value and sets the held LAPIC
state again before any vCPU runs, and then starts the VM. Resuming a running
VM returns `OK` and changes nothing. Resuming a VM that is stopped for another
reason is `REJECTED`. If OpenVMM cannot restore the held guest time, the status
is `REJECTED`, the VM stays `PAUSED` with its held time, and the host may
retry. If the VM cannot start, the status is `FAILED`.

## Guest time across a pause

The guest observes no elapsed monotonic time across a host pause. The TSC, the
LAPIC timers, and the VM time that drives emulated timers, such as the PIT and
the RTC's periodic and alarm interrupts, all stop with the vCPUs and continue
from the same values.

Wall-clock time is not held. The CMOS RTC's date and time follow host UTC, so
after a resume they read the current host time. The guest's system clock is
behind host UTC by the paused duration until the guest steps it, for example
from the RTC. The NVX guest's wall-clock discipline does this at its next
poll.

## Guest activity while paused

While the VM is paused, nothing that needs the guest makes progress. Guest
timers, guest-requested snapshots, and control-console traffic wait for the
resume. A guest snapshot request that is still pending when a pause stops the
vCPUs is cancelled, as when the VM stops for any other reason. A request that
already reached its snapshot boundary makes the pause `BUSY` instead.

If OpenVMM processes a guest-requested reset while the host holds a pause, the
reset discards the held guest time but keeps the pause. The reset VM starts at
the next `RESUME`. If the reset fails, the pause ends and the VM stays
`STOPPED`, so a `RESUME` is `REJECTED`.
