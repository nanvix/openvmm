# Restore Component Coverage

| Component | Stable identity / source | Restore mode | Required postcondition | Evidence |
| --- | --- | --- | --- | --- |
| Partition | `partition` | Deferred stateful | Partition blob restored before VP blobs | `vmm_core/src/partition_unit.rs`, `PartitionUnitRunner::restore` |
| vCPU | `vp<N>`; snapshot records form a prefix of the destination VP capacity | Deferred stateful | Available selected snapshot VPs are restored; the remaining abstract destination VP state stays initial/default. Current backend and inventory restrictions may require a full-capacity snapshot and must be established by the production proof rather than assumed by the TOP projection. | `vmm_core/src/partition_unit/vp_set.rs` |
| VM time | `vmtime` | Immediate stateful | Saved `VmTime` plus truncated 100ns downtime, wrapping to `u64` | `vm/vmcore/src/vmtime.rs`, `vmtime_unit.rs` |
| Input distributor | `input` | Stateless | No saved blob; fresh runtime state | `openvmm/openvmm_core/src/input_distributor.rs`, `StateUnit` implementation |
| RTC | `rtc` | Immediate stateful | CMOS/address/transaction state restored; local clock advanced by checked whole milliseconds | `vm/devices/chipset/src/cmos_rtc.rs` |
| PIC/IOAPIC/LAPIC/PIT | stable manifest names | Immediate stateful | Saved interrupt/timer state restored; LAPIC downtime mutations follow production formulas | `openvmm/openvmm_helpers/src/snapshot.rs`, `vmm_core/virt/src/x86/vp.rs` |
| Virtio transport | configuration-derived state-unit name | Deferred private state | Kept outside the core-restored set; common transport and queue progress are retained as pending state, private payload preserves `Pending(None)` versus `NotRestored`, queues are not started, and fresh pending kicks are false | `vm/devices/virtio/virtio/src/transport/core.rs`, `task.rs` |
| VMBus/VTL2 VMBus | `vmbus` / `vtl2_vmbus` when configured | Conditional stateful | Connected/disconnected channel state, GPADLs, and pending messages restored | `vm/devices/vmbus/vmbus_server/src/channels/saved_state.rs` |
| RAM backing | manifest memory ranges | Prepared upstream input | Every declared GPA maps to the correct memory-file offset; holes remain unmapped | `openvmm/openvmm_helpers/src/snapshot.rs` |
| VMGS/disks/attachments | manifest/config logical identity | External contract | Reopened resource has the approved content and attachment identity; FD equality is irrelevant | snapshot preparation and resource resolver call sites |

## Time and APIC details

- `VmTime` is a `u64` count of 100ns units. `wrapping_add(Duration)` adds `duration.as_nanos() / 100`, truncating sub-100ns duration and narrowing modulo `2^64`.
- Generic `Partition::advance_snapshot_time` is a no-op. KVM reads its nanosecond clock, converts `Duration::as_nanos()` to `u64`, checks addition overflow, and writes the requested clock.
- TSC advancement uses elapsed cycles derived from downtime and the saved frequency. The operation reads all relevant vCPU state before publishing through the access-state commit, but failure across multiple VPs can still leave earlier VP commits applied; the top-level error contract therefore promises non-publication/non-execution, not rollback.
- LAPIC DCR codes map `0xb,0,1,2,3,8,9,0xa` to shifts `0..7`. Masked timers or vectors below 16 do not queue an interrupt. Expiry sets IRR and clears the corresponding TMR and auto-EOI bits. Periodic mode reloads CCR using `period - remaining % period`; one-shot clears CCR; TSC-deadline expiry clears the deadline.

## Explicitly unclosed production bridges

1. Snapshot opening, manifest validation, saved-state decoding, and memory-file mapping establish the preconditions supplied to `InitializedVm::load`; they are outside this TOP theorem.
2. Resource resolver outputs satisfy recorded disk, VMGS, and attachment identities.
3. State-unit inventory validation and asynchronous restore imply the per-component relations above.
4. Hypervisor save/restore implementations preserve the opaque architecture fields required by the selected capability contract.
5. Component Views for processor topology, partition/VP state, state units, memory, compatibility, and resources establish the fields composed by the standard `InitializedVm@` and `LoadedVm@` Views.
6. The `LoadedVm` View and stop-guard representation establish the `PreExecutionRestored` boundary. The two callers, `VmWorker::new` and `VmWorker::restart`, must separately establish that a failed `load` returns before `LOADED_VM.store`, readiness publication, or `resume`.

These are required proof obligations. The production View bridges are temporarily represented by the narrowly scoped `uninterp spec fn` declarations recorded in `UNINTERP.json`; no `assume`, `admit`, `external_body`, copied executable, or uninterpreted predicate that directly asserts the final theorem is used.
