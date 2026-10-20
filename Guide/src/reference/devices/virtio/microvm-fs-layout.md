# microVM filesystem layout

The fixed microVM virtio-fs device exposes a versioned, read-only attachment
layout without relying on bootstrap command-line tokens.

## Platform-template contract

New `platform` captures record `platform_template_abi_version = 1` in field
34 of `SnapshotMachineContract`. The existing microVM ABI and boot-layout
versions remain 2. A version-1 platform template must contain the dormant
`fs:microvm0` device, with its existing tag, MMIO window, IRQ, feature banks,
queues, and state-unit identity. Its saved command line contains no
`virtfs_*` attachment tokens.

On restore, no filesystem options leave that slot dormant. A `--mount` or
`--mount-aggregate` with 1 to 256 `--mount-child` arguments populates its
backend before any restored vCPU runs. Each child's canonical host path,
identity, file/directory kind, access mode, and denied/allowed/writable
policy pass the same validation as a cold aggregate. Platform-template
ownership remains `vmm`. There is no host-root fallback or device hotplug.

The new permission applies only to the versioned platform contract.
Workload and older snapshots keep their existing exact-match rules;
an older dormant-slot snapshot still accepts only one new `--mount`.
Restoring a captured active aggregate still requires the exact saved
children, paths, identities, order, modes, owner, and policies.

Version 1 is a shared template-contract envelope, not permission to change
other devices. Its immutable compatibility key must still include the exact
device inventory, CPU/RAM geometry, upper/scratch mode, scratch capacity and
sector geometry, saved NIC addressing, guest ABI, and activation schema.
Bound image identity and future media capacity are not template-key inputs.
A sole-image-slot template
and a fixed-NIC template must be composed and tested at one integrated
source revision; neither can be added to an older template during restore.
The filesystem primitive does not implement those other devices.

## Device config schema

The standard 36-byte tag and 4-byte request-queue count are unchanged.
The layout starts at device-config offset 40, or MMIO
`0xd0001000 + 0x100 + 40`. All multibyte fields are little-endian.
The first 32 bytes contain:

| Relative offset | Type | Meaning |
|---|---|---|
| 0 | u32 | Magic `0x5346564d` (`MVFS`) |
| 4 | u32 | Layout version, 1 |
| 8 | u32 | Header size, 32 |
| 12 | u32 | State: 0 dormant, 1 single directory, 2 aggregate |
| 16 | u32 | Access: 0 dormant, 1 read-only, 2 read-write |
| 20 | u32 | Owner: 0 VMM, 1 caller with root squashed |
| 24 | u32 | Child count: 0 dormant/single, 1 to 256 aggregate |
| 28 | u32 | Child record size, 4 |

The aggregate's access field is read-write when any child is read-write;
its synthetic root itself remains read-only. Immediately after the header,
each aggregate child occupies one 4-byte record:

| Relative offset | Type | Meaning |
|---|---|---|
| 0 | u16 | Zero-based index in attachment and receipt order |
| 2 | u8 | Kind: 1 directory, 2 regular file |
| 3 | u8 | Access: 1 read-only, 2 read-write |

The maximum config size is `40 + 32 + 256 * 4 = 1096` bytes, within the
existing MMIO window. Writes cannot change the layout. A standard-machine
virtio-fs device does not expose this extension. Schema constants live in
`virtio_resources::fs::microvm::layout`.

Discovery works before FUSE negotiation and does not depend on
`virtfs_aggregate=1`, modified restore boot arguments, a second transport,
or a shared configuration region. Guest code must reject unsupported
versions, sizes, state/mode combinations, duplicate or unordered indices,
and counts that disagree with its authenticated activation.

## Activation and attachment receipts

Device config is discovery, not an authorization or a host-inode attestation.
The existing authenticated activation RPC must carry the opened-attachment
provider's receipt alongside the expected mapping count, including zero.
The receipt interface must bind:

- The activation's generation/config identity, stable device ID, tag, and
  layout version, owner, aggregate access, and exact child count.
- Each ordered child's index and name, kind, mode, denied/allowed/writable
  policy, object-pinned or explicit live-path grant, and observed opened
  host-object identity from the attachment provider.

Indices identify the ordered `--mount-child` records, not guest-chosen
paths. Names come from the authenticated receipt, so arbitrary valid child
names remain supported without putting host paths or identities in device
config. The guest compares indices, kinds, and modes with this layout,
then uses those authenticated names for lookups and safe binds. A mismatch
or partial activation must fail before Ready; zero mappings require a
dormant layout and no mount or bind batch.

This primitive neither emits opened-object receipts nor defines a new RPC.
The opened-attachment provider must verify actual handles, retain pinned
objects rather than follow replacement names, and authenticate its receipt
through the runtime's activation. The filesystem's existing live-path
semantics are not an object-pinned guarantee.

Real-guest acceptance must cover discovery, mounts, safe binds and receipt
matching through that RPC, including zero/one/max and mixed directory/file
read-only/read-write mappings, at the combined source pin. Device transport
and contract tests alone do not prove a guest-activated platform restore.
