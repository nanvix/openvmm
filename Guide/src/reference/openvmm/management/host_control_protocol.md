# Host-control Protocol

The microVM host-control service is a host-only, reconnectable endpoint for
versioned runtime operations. Linux uses an owner-only Unix socket in an
owner-only directory. Windows uses a named pipe restricted to the local
system and the OpenVMM process owner. OpenVMM also verifies the connected
peer's UID or SID.

The endpoint is selected with `--microvm-host-control listen=<PATH>`.
`--microvm-control-auth-stdin` supplies one exact, nonzero 32-byte capability
through a prepared one-way pipe. The same capability authenticates the guest
control console when both endpoints are enabled. It is never accepted through
argv or the environment.

After connecting, the client writes the raw 32-byte capability. Authentication
must complete within the configured control-auth timeout. Requests and
responses then use a little-endian `u32` byte length followed by UTF-8 JSON.
Frames are limited to 64 KiB, and an idle connection closes after 60 seconds.
Every request contains `version: 1`, a caller-selected `request_id`, and an
`operation`. Responses repeat the protocol version and request ID and contain
either `status: "ok"` or a stable error code and message.

Protocol version 1 defines:

- `query_image_slots`: returns all four slots as `inactive`, `empty`, or
  `bound`, including the recorded identity and capacity.
- `bind_image_slot`: supplies `slot`, `path`, and `identity`. OpenVMM opens the
  path itself as a read-only, non-symlink regular file. The active empty slot
  accepts exactly one 512-byte-aligned image, updates capacity, and raises a
  virtio configuration-change interrupt. Repeating the same identity is
  idempotent. A different identity, inactive slot, invalid geometry, or
  lifecycle transition fails explicitly.

There is no unbind, eject, rebind, activation, device-add, or device-remove
operation. Media remains open until the VM exits. Snapshot capture and other
lifecycle transitions exclude binding.
