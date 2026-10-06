// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio block device implementation.

#![forbid(unsafe_code)]

pub mod resolver;

#[cfg(test)]
mod integration_tests;

use anyhow::Context as _;
use disk_backend::Disk;
use disk_file::FileDisk;
use futures::StreamExt;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use pal_async::wait::PolledWait;
use parking_lot::RwLock;
use scsi_buffers::RequestBuffers;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use unicycle::FuturesUnordered;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::regions::DataRegion;
use virtio::regions::data_regions;
use virtio::regions::try_build_gpn_list;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::blk::*;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Maximum number of segments per request advertised via `seg_max` (spec §5.2.4).
///
/// This is the maximum number of data descriptors (excluding header and
/// status) in a single request. The virtio spec requires that a
/// descriptor chain is no longer than the queue size, and each block
/// request uses one descriptor for the header and one for the status
/// byte, so the data segment limit is `DEFAULT_QUEUE_SIZE - 2`.
const DEFAULT_SEG_MAX: u32 = virtio::DEFAULT_QUEUE_SIZE as u32 - 2;

const MAX_IO_DEPTH: usize = 64;

/// The virtio-blk device.
#[derive(InspectMut)]
pub struct VirtioBlkDevice {
    #[inspect(flatten, mut)]
    worker: TaskControl<BlkWorker, BlkQueueState>,
    #[inspect(skip)]
    driver: VmTaskDriver,
    read_only: bool,
    supports_discard: bool,
    config: VirtioBlkConfig,
    media: Arc<RwLock<MediaState>>,
    image_slot: Option<ImageSlot>,
}

#[derive(Inspect)]
struct MediaState {
    #[inspect(with = "Option::is_some")]
    disk: Option<Disk>,
    identity: Option<String>,
}

#[derive(Inspect)]
struct ImageSlot {
    active: bool,
    #[inspect(skip)]
    requests: mesh::Receiver<virtio_resources::blk::ImageSlotRequest>,
}

/// Persistent worker state. Survives across enable/disable cycles.
///
/// Holds the disk backend, stats counters, and the
/// `FuturesUnordered` that tracks in-flight IOs. The IO futures
/// live here (not in `BlkQueueState`) so they survive when the
/// task is stopped — they're drained in `poll_disable()` before
/// the queue state is removed.
#[derive(InspectMut)]
struct BlkWorker {
    media: Arc<RwLock<MediaState>>,
    read_only: bool,
    stats: WorkerStats,
    #[inspect(with = "FuturesUnordered::len")]
    ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoCompletion> + Send>>>,
}

/// Transient queue state, created in `enable()` and removed in `poll_disable()`.
#[derive(InspectMut)]
struct BlkQueueState {
    queue: VirtioQueue,
    memory: GuestMemory,
}

#[derive(Inspect, Default)]
struct WorkerStats {
    read_ops: Counter,
    write_ops: Counter,
    flush_ops: Counter,
    discard_ops: Counter,
    bounce_ops: Counter,
    errors: Counter,
}

impl InspectTaskMut<BlkQueueState> for BlkWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut BlkQueueState>) {
        req.respond().merge(self).merge(state);
    }
}

/// Result of a completed IO operation, returned from the spawned future
/// back to the main task for stats accumulation and descriptor completion.
struct IoCompletion {
    work: VirtioQueueCallbackWork,
    bytes_written: u32,
    stat: IoStat,
    bounced: bool,
}

/// Which stat counter to increment for a completed IO.
enum IoStat {
    Read,
    Write,
    Flush,
    Discard,
    Error,
    None,
}

impl BlkWorker {
    /// Complete a descriptor and accumulate stats.
    fn finish_io(&mut self, queue: &mut VirtioQueue, completion: IoCompletion) {
        queue.complete(completion.work, completion.bytes_written);
        match completion.stat {
            IoStat::Read => self.stats.read_ops.increment(),
            IoStat::Write => self.stats.write_ops.increment(),
            IoStat::Flush => self.stats.flush_ops.increment(),
            IoStat::Discard => self.stats.discard_ops.increment(),
            IoStat::Error => self.stats.errors.increment(),
            IoStat::None => {}
        }
        if completion.bounced {
            self.stats.bounce_ops.increment();
        }
    }

    /// Poll all in-flight IOs to completion.
    ///
    /// Called during `poll_disable()` after the worker task has been stopped.
    /// The `FuturesUnordered` still holds any IOs that were in flight when
    /// `until_stopped` returned. This drains them, ensuring all descriptor
    /// completions are written to the used ring before the queue is dropped.
    fn poll_drain(&mut self, queue: &mut VirtioQueue, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match self.ios.poll_next_unpin(cx) {
                Poll::Ready(Some(completion)) => self.finish_io(queue, completion),
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncRun<BlkQueueState> for BlkWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut BlkQueueState,
    ) -> Result<(), task_control::Cancelled> {
        // Set once the queue can no longer produce work, because the guest
        // violated the queue protocol. Fetching stops, but in-flight IOs
        // keep draining so their descriptors are still completed; the
        // worker exits once they are done.
        let mut queue_done = false;

        while !(queue_done && self.ios.is_empty()) {
            enum Event {
                NewWork(Result<VirtioQueueCallbackWork, std::io::Error>),
                Completed(IoCompletion),
            }

            let event = poll_fn(|cx| {
                // Check cancellation first so a continuously ready queue
                // cannot starve a save/restore or teardown stop request.
                if Pin::new(&mut *stop).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }

                // Poll for completed IOs first to free up slots.
                if let Poll::Ready(Some(completion)) = self.ios.poll_next_unpin(cx) {
                    return Poll::Ready(Some(Event::Completed(completion)));
                }
                // Accept new work if under the depth limit.
                if !queue_done && self.ios.len() < MAX_IO_DEPTH {
                    if let Poll::Ready(item) = state.queue.poll_next_unpin(cx) {
                        let item = item.expect("virtio queue stream never ends");
                        return Poll::Ready(Some(Event::NewWork(item)));
                    }
                }
                Poll::Pending
            })
            .await
            .ok_or(task_control::Cancelled)?;

            match event {
                Event::NewWork(Ok(work)) => {
                    let disk = self.media.read().disk.clone();
                    let mem = state.memory.clone();
                    let read_only = self.read_only;
                    self.ios.push(Box::pin(async move {
                        process_request(disk.as_ref(), &mem, read_only, work).await
                    }));
                }
                Event::NewWork(Err(err)) => {
                    // The queue is retired: the rejected chain is never
                    // consumed, so retrying would fail identically forever.
                    tracelimit::error_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "error reading from virtio queue, stopping worker"
                    );
                    queue_done = true;
                }
                Event::Completed(completion) => {
                    self.finish_io(&mut state.queue, completion);
                }
            }
        }

        Ok(())
    }
}

impl VirtioBlkDevice {
    /// Creates a new virtio-blk device backed by the given disk.
    pub fn new(driver_source: &VmTaskDriverSource, disk: Disk, read_only: bool) -> Self {
        let sector_count = disk.sector_count();
        let sector_size = disk.sector_size();
        let physical_sector_size = disk.physical_sector_size();

        let physical_block_exp = if physical_sector_size > sector_size {
            (physical_sector_size / sector_size).trailing_zeros() as u8
        } else {
            0
        };

        // Virtio block config space (spec §5.2.4).
        //
        // `capacity` is always present. Other fields are gated by feature bits
        // we advertise in `traits()`.
        let config = VirtioBlkConfig {
            // Capacity in 512-byte sectors (spec §5.2.4). The protocol always
            // uses 512-byte units regardless of the disk's native sector size.
            capacity: sector_count * (sector_size as u64 / 512),
            // Maximum bytes in a single segment (VIRTIO_BLK_F_SIZE_MAX). Not
            // specified.
            size_max: 0,
            // Maximum segments per request (VIRTIO_BLK_F_SEG_MAX).
            seg_max: DEFAULT_SEG_MAX,
            // CHS geometry (VIRTIO_BLK_F_GEOMETRY) — not advertised, zeroed.
            geometry: VirtioBlkGeometry {
                cylinders: 0,
                heads: 0,
                sectors: 0,
            },
            // Native logical block size (VIRTIO_BLK_F_BLK_SIZE, spec §5.2.5).
            // Doesn't change protocol units but lets the driver align I/O.
            blk_size: sector_size,
            // Topology (VIRTIO_BLK_F_TOPOLOGY, spec §5.2.5 step 4).
            topology: VirtioBlkTopology {
                physical_block_exp,
                alignment_offset: 0,
                // Suggested minimum I/O size in logical blocks. 1 = no constraint.
                min_io_size: 1,
                // Optimal (max) I/O size in logical blocks. 0 = no hint.
                opt_io_size: 0,
            },
            // We don't advertise CONFIG_WCE; set writeback=1 to indicate
            // writeback cache semantics (driver should use FLUSH).
            writeback: 1,
            unused0: 0,
            // We don't advertise MQ; single queue.
            num_queues: 1,
            // Discard fields (VIRTIO_BLK_F_DISCARD, spec §5.2.4).
            // u32::MAX × 512 bytes ≈ 2 TiB per segment; no practical limit.
            max_discard_sectors: u32::MAX,
            max_discard_seg: 1,
            // Alignment in 512-byte sectors for discard ranges. Uses the
            // backend's optimal unmap granularity (same as SCSI Optimal
            // Unmap Granularity), converted to 512-byte units.
            discard_sector_alignment: disk.optimal_unmap_sectors() * (sector_size / 512),
            // Write zeroes fields (VIRTIO_BLK_F_WRITE_ZEROES) — not advertised.
            max_write_zeroes_sectors: 0,
            max_write_zeroes_seg: 0,
            write_zeroes_may_unmap: 0,
            unused1: [0; 3],
            _padding: [0; 4],
        };

        let supports_discard = disk.unmap_behavior() != disk_backend::UnmapBehavior::Ignored;

        let media = Arc::new(RwLock::new(MediaState {
            disk: Some(disk),
            identity: None,
        }));
        Self {
            worker: TaskControl::new(BlkWorker {
                media: media.clone(),
                read_only,
                stats: WorkerStats::default(),
                ios: FuturesUnordered::new(),
            }),
            driver: driver_source.simple(),
            read_only,
            supports_discard,
            config,
            media,
            image_slot: None,
        }
    }

    /// Creates a fixed-geometry image slot with no media.
    pub fn new_image_slot(
        driver_source: &VmTaskDriverSource,
        active: bool,
        requests: mesh::Receiver<virtio_resources::blk::ImageSlotRequest>,
    ) -> Self {
        let media = Arc::new(RwLock::new(MediaState {
            disk: None,
            identity: None,
        }));
        let config = VirtioBlkConfig {
            capacity: 0,
            size_max: 0,
            seg_max: DEFAULT_SEG_MAX,
            geometry: VirtioBlkGeometry {
                cylinders: 0,
                heads: 0,
                sectors: 0,
            },
            blk_size: 512,
            topology: VirtioBlkTopology {
                physical_block_exp: 3,
                alignment_offset: 0,
                min_io_size: 1,
                opt_io_size: 0,
            },
            writeback: 1,
            unused0: 0,
            num_queues: 1,
            max_discard_sectors: 0,
            max_discard_seg: 0,
            discard_sector_alignment: 0,
            max_write_zeroes_sectors: 0,
            max_write_zeroes_seg: 0,
            write_zeroes_may_unmap: 0,
            unused1: [0; 3],
            _padding: [0; 4],
        };
        Self {
            worker: TaskControl::new(BlkWorker {
                media: media.clone(),
                read_only: true,
                stats: WorkerStats::default(),
                ios: FuturesUnordered::new(),
            }),
            driver: driver_source.simple(),
            read_only: true,
            supports_discard: false,
            config,
            media,
            image_slot: Some(ImageSlot { active, requests }),
        }
    }

    fn image_slot_state(&self) -> virtio_resources::blk::ImageSlotState {
        let media = self.media.read();
        virtio_resources::blk::ImageSlotState {
            identity: media.identity.clone(),
            capacity_sectors: media.disk.as_ref().map_or(0, |disk| {
                disk.sector_count() * (disk.sector_size() as u64 / 512)
            }),
        }
    }

    fn bind_image_slot(
        &self,
        request: virtio_resources::blk::BindImageSlotRequest,
    ) -> Result<virtio_resources::blk::ImageSlotState, virtio_resources::blk::BindImageSlotError>
    {
        use virtio_resources::blk::BindImageSlotError;

        if request.identity.is_empty() || request.identity.len() > 1024 {
            return Err(BindImageSlotError::InvalidMedia(
                "identity must contain 1 to 1024 bytes".to_owned(),
            ));
        }
        if request.logical_block_size != 512 || request.physical_block_size != 4096 {
            return Err(BindImageSlotError::GeometryMismatch);
        }
        let metadata = request
            .media
            .metadata()
            .map_err(|error| BindImageSlotError::InvalidMedia(error.to_string()))?;
        if !metadata.file_type().is_file()
            || request.length == 0
            || request.length % request.logical_block_size as u64 != 0
            || metadata.len() != request.length
        {
            return Err(BindImageSlotError::InvalidMedia(
                "media must be a nonempty aligned regular file".to_owned(),
            ));
        }

        let mut media = self.media.write();
        if let Some(identity) = &media.identity {
            return if identity == &request.identity {
                Ok(virtio_resources::blk::ImageSlotState {
                    identity: Some(identity.clone()),
                    capacity_sectors: media.disk.as_ref().map_or(0, Disk::sector_count),
                })
            } else {
                Err(BindImageSlotError::AlreadyBound)
            };
        }
        let disk = Disk::new(FileDisk::with_metadata(
            request.media,
            disk_file::Metadata {
                disk_size: request.length,
                sector_size: request.logical_block_size,
                physical_sector_size: request.physical_block_size,
                read_only: true,
            },
        ))
        .map_err(|error| BindImageSlotError::InvalidMedia(error.to_string()))?;
        media.disk = Some(disk);
        media.identity = Some(request.identity);
        Ok(virtio_resources::blk::ImageSlotState {
            identity: media.identity.clone(),
            capacity_sectors: request.length / 512,
        })
    }
}

impl VirtioDevice for VirtioBlkDevice {
    fn traits(&self) -> DeviceTraits {
        let mut features = VIRTIO_BLK_F_SEG_MAX
            | VIRTIO_BLK_F_BLK_SIZE
            | VIRTIO_BLK_F_FLUSH
            | VIRTIO_BLK_F_TOPOLOGY;

        if self.read_only {
            features |= VIRTIO_BLK_F_RO;
        }
        if self.supports_discard {
            features |= VIRTIO_BLK_F_DISCARD;
            // FUTURE: investigate adding VIRTIO_BLK_F_WRITE_ZEROES support
            // by adding an explicit write_zeroes operation to the DiskIo
            // backend trait, rather than emulating it with bounce-buffer writes.
        }

        DeviceTraits {
            device_id: if self.image_slot.as_ref().is_some_and(|slot| !slot.active) {
                virtio::spec::VirtioDeviceType(0)
            } else {
                virtio::spec::VirtioDeviceType::BLK
            },
            device_features: VirtioDeviceFeatures::new()
                .with_device_specific_low(features)
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1,
            // Config space is 60 bytes (size_of minus 4 bytes of struct padding).
            device_register_length: (size_of::<VirtioBlkConfig>() - 4) as u32,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        // The transport reads the device config space as a sequence of u32s.
        // We serialize VirtioBlkConfig to bytes and return the requested
        // 4-byte window. Three cases:
        let mut config = self.config.clone();
        if self.image_slot.is_some() {
            config.capacity = self.image_slot_state().capacity_sectors;
        }
        let config_bytes = config.as_bytes();
        let offset = offset as usize;
        if offset + 4 <= config_bytes.len() {
            // Normal case: full u32 within bounds.
            u32::from_le_bytes(config_bytes[offset..offset + 4].try_into().unwrap())
        } else if offset < config_bytes.len() {
            // Partial read at the end of config space: zero-pad the
            // remaining bytes so the transport always gets a full u32.
            let mut bytes = [0u8; 4];
            let len = config_bytes.len() - offset;
            bytes[..len].copy_from_slice(&config_bytes[offset..]);
            u32::from_le_bytes(bytes)
        } else {
            // Completely out of range: return zero.
            0
        }
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Config space is read-only for virtio-blk.
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        assert_eq!(idx, 0);

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create queue event")?;

        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.worker.insert(
            self.driver.clone(),
            "virtio-blk-worker",
            BlkQueueState {
                queue,
                memory: resources.guest_memory,
            },
        );
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        assert_eq!(idx, 0);
        if !self.worker.has_state() {
            return None;
        }
        // Stop the worker task (cancels the run loop via until_stopped).
        self.worker.stop().await;
        // Drain in-flight IOs to completion. The FuturesUnordered lives in
        // BlkWorker and survives the stop — its pending disk IO futures are
        // polled here until all descriptors are completed in the used ring.
        let (worker, queue_state) = self.worker.get_mut();
        let queue = &mut queue_state.expect("state exists after stop").queue;
        poll_fn(|cx| worker.poll_drain(queue, cx)).await;
        // Remove the queue state (drops VirtioQueue).
        let state = self.worker.remove().queue.queue_state();
        Some(state)
    }

    fn supports_save_restore(&self) -> bool {
        true
    }

    fn poll_config_change(&mut self, cx: &mut Context<'_>) -> Poll<anyhow::Result<bool>> {
        let Some(slot) = &mut self.image_slot else {
            return Poll::Pending;
        };
        let request = match slot.requests.poll_next_unpin(cx) {
            Poll::Ready(Some(request)) => request,
            Poll::Ready(None) | Poll::Pending => return Poll::Pending,
        };
        match request {
            virtio_resources::blk::ImageSlotRequest::Bind(rpc) => {
                let was_empty = self.image_slot_state().identity.is_none();
                rpc.handle_sync(|request| self.bind_image_slot(request));
                let changed = was_empty && self.image_slot_state().identity.is_some();
                Poll::Ready(Ok(changed))
            }
            virtio_resources::blk::ImageSlotRequest::Query(rpc) => {
                rpc.handle_sync(|()| self.image_slot_state());
                Poll::Ready(Ok(false))
            }
        }
    }
}

/// Process a single virtio-blk request.
///
/// Returns the work item back with completion info so the caller can
/// write the used ring entry. This keeps completion in the main loop,
/// which simplifies future queue API changes.
async fn process_request(
    disk: Option<&Disk>,
    mem: &GuestMemory,
    read_only: bool,
    work: VirtioQueueCallbackWork,
) -> IoCompletion {
    let Some(disk) = disk else {
        if let Err(err) = write_status_byte(mem, &work, VIRTIO_BLK_S_IOERR) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failed to write error status byte"
            );
        }
        return IoCompletion {
            work,
            bytes_written: 1,
            stat: IoStat::Error,
            bounced: false,
        };
    };
    match process_request_inner(disk, mem, read_only, &work).await {
        Ok((bytes_written, stat, bounced)) => {
            if let Err(err) = write_status_byte(mem, &work, VIRTIO_BLK_S_OK) {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write status byte"
                );
            }
            IoCompletion {
                work,
                bytes_written: bytes_written + 1, // +1 for status byte
                stat,
                bounced,
            }
        }
        Err(status) => {
            if let Err(err) = write_status_byte(mem, &work, status) {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "failed to write error status byte"
                );
            }
            IoCompletion {
                work,
                bytes_written: 1, // just the status byte
                stat: IoStat::Error,
                bounced: false,
            }
        }
    }
}

/// Inner request processing. Returns Ok((data_bytes_written, stat, bounced)) on
/// success, or Err(status_code) on failure.
async fn process_request_inner(
    disk: &Disk,
    mem: &GuestMemory,
    read_only: bool,
    work: &VirtioQueueCallbackWork,
) -> Result<(u32, IoStat, bool), u8> {
    // Read the request header from the first (readable) descriptor.
    let mut header = VirtioBlkReqHeader::new_zeroed();
    let header_len = work
        .read(mem, header.as_mut_bytes())
        .map_err(|_| VIRTIO_BLK_S_IOERR)?;

    if header_len < size_of_val(&header) {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let request_type = header.request_type;
    // Shift to convert 512-byte virtio sectors to backend disk sectors.
    // Only meaningful for commands that use sector addressing (IN, OUT, DISCARD).
    let sector_shift = disk.sector_shift() - 9;
    let sector_mask = (1u64 << sector_shift) - 1; // alignment mask for validation

    match request_type {
        VIRTIO_BLK_T_IN => {
            let disk_sector = virtio_to_disk_sector(header.sector, sector_shift, sector_mask)?;
            let (bytes, bounced) = do_io(disk, mem, work, disk_sector, true).await?;
            Ok((bytes, IoStat::Read, bounced))
        }
        VIRTIO_BLK_T_OUT => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let disk_sector = virtio_to_disk_sector(header.sector, sector_shift, sector_mask)?;
            let (bytes, bounced) = do_io(disk, mem, work, disk_sector, false).await?;
            Ok((bytes, IoStat::Write, bounced))
        }
        VIRTIO_BLK_T_FLUSH => {
            disk.sync_cache().await.map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Flush, false))
        }
        VIRTIO_BLK_T_GET_ID => {
            let id = if let Some(disk_id) = disk.disk_id() {
                let mut id_str = [0u8; VIRTIO_BLK_ID_BYTES];
                let hex: String = disk_id.iter().map(|b| format!("{:02x}", b)).collect();
                let copy_len = hex.len().min(VIRTIO_BLK_ID_BYTES);
                id_str[..copy_len].copy_from_slice(&hex.as_bytes()[..copy_len]);
                id_str
            } else {
                *b"openvmm-virtio-blk\0\0"
            };
            work.write(mem, &id).map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((VIRTIO_BLK_ID_BYTES as u32, IoStat::None, false))
        }
        VIRTIO_BLK_T_DISCARD => {
            if read_only {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            // Per spec §5.2.6.1: "The unmap bit MUST be zero for discard commands."
            // Per spec §5.2.6.2: "the device MAY deallocate the specified range."
            // Discard is a hint — no data-content guarantee.

            /// Combined header + discard/write-zeroes segment, used to read
            /// the full request in one `work.read()` call.
            #[repr(C)]
            #[derive(
                zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable, zerocopy::KnownLayout,
            )]
            struct VirtioBlkDiscardReq {
                header: VirtioBlkReqHeader,
                seg: VirtioBlkDiscardWriteZeroes,
            }

            let mut discard_req = VirtioBlkDiscardReq::new_zeroed();
            let read_len = work
                .read(mem, discard_req.as_mut_bytes())
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            if read_len < size_of_val(&discard_req) {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let seg = discard_req.seg;
            // Spec §5.2.6.2: "the device MUST set the status byte to
            // VIRTIO_BLK_S_UNSUPP for discard commands if the unmap flag is set."
            if seg.flags != 0 {
                return Err(VIRTIO_BLK_S_UNSUPP);
            }
            // Discard segment has its own sector/count fields (in 512-byte units).
            let disk_sector = virtio_to_disk_sector(seg.sector, sector_shift, sector_mask)?;
            let num_sectors = seg.num_sectors as u64;
            if num_sectors & sector_mask != 0 {
                return Err(VIRTIO_BLK_S_IOERR);
            }
            let disk_count = num_sectors >> sector_shift;
            disk.unmap(disk_sector, disk_count, false)
                .await
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
            Ok((0, IoStat::Discard, false))
        }
        _ => Err(VIRTIO_BLK_S_UNSUPP),
    }
}

/// Convert a 512-byte virtio sector number to a backend disk sector,
/// validating alignment for disks with larger native sectors.
fn virtio_to_disk_sector(
    virtio_sector: u64,
    sector_shift: u32,
    sector_mask: u64,
) -> Result<u64, u8> {
    if virtio_sector & sector_mask != 0 {
        return Err(VIRTIO_BLK_S_IOERR);
    }
    Ok(virtio_sector >> sector_shift)
}

/// Owned data needed to construct a `PagedRange`.
///
/// `PagedRange` borrows its GPN slice, so we need to keep the `Vec` alive.
/// This struct holds the owned data alongside the offset and length.
struct OwnedPagedRange {
    gpns: Vec<u64>,
    offset: usize,
    len: usize,
}

/// Copy data between scattered guest regions and a contiguous bounce buffer.
///
/// When `to_bounce` is true (guest→bounce, before write IO), reads from
/// `guest_mem` at scattered `region.addr` addresses and writes
/// contiguously into `bounce_buf`.
///
/// When `to_bounce` is false (bounce→guest, after read IO), reads
/// contiguously from `bounce_buf` and writes to `guest_mem` at
/// scattered `region.addr` addresses.
fn copy_regions(
    bounce_buf: &mut [u8],
    guest_mem: &GuestMemory,
    regions: &[DataRegion],
    to_bounce: bool,
) -> Result<(), u8> {
    let mut linear_offset: usize = 0;
    for region in regions {
        let len = region.len as usize;
        let buf = &mut bounce_buf[linear_offset..linear_offset + len];
        if to_bounce {
            guest_mem
                .read_at(region.addr, buf)
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        } else {
            guest_mem
                .write_at(region.addr, buf)
                .map_err(|_| VIRTIO_BLK_S_IOERR)?;
        }
        linear_offset += len;
    }
    Ok(())
}

/// Perform read or write I/O for a single request.
///
/// Issues a single `disk.read_vectored` / `disk.write_vectored` call
/// for the entire data payload. If the descriptor chain's memory
/// layout is compatible with [`PagedRange`] (all interior boundaries
/// page-aligned or GPA-contiguous), the IO targets guest memory
/// directly. Otherwise, a bounce buffer is allocated and data is
/// copied through it.
///
/// Returns `Ok((data_bytes_written_to_guest, bounced))` on success.
async fn do_io(
    disk: &Disk,
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    start_disk_sector: u64,
    is_read: bool,
) -> Result<(u32, bool), u8> {
    let writable = is_read;
    let total_payload = work.get_payload_length(writable);
    let skip_bytes: u64 = if !is_read {
        size_of::<VirtioBlkReqHeader>() as u64
    } else {
        0
    };
    let data_len = if is_read {
        total_payload.saturating_sub(1)
    } else {
        total_payload.saturating_sub(skip_bytes)
    };

    if data_len == 0 {
        return Ok((0, false));
    }

    // Cap request size to prevent u32 overflow when reporting bytes_written
    // (the caller adds +1 for the status byte). No legitimate IO should be
    // anywhere near this limit.
    if data_len > u32::MAX as u64 - 1 {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    // Validate that the data length is a whole number of backend sectors.
    // The disk backend may panic if given a non-sector-aligned buffer.
    let sector_size = disk.sector_size() as u64;
    if data_len & (sector_size - 1) != 0 {
        return Err(VIRTIO_BLK_S_IOERR);
    }

    let regions: Vec<_> = data_regions(&work.payload, writable, skip_bytes, data_len).collect();

    let (mut io_mem, io_range, bounced) =
        if let Some((gpns, offset, len)) = try_build_gpn_list(&regions) {
            // Fast path: descriptor chain is PagedRange-compatible.
            (None, OwnedPagedRange { gpns, offset, len }, false)
        } else {
            // Slow path: allocate a bounce buffer.
            // TODO: cap data_len to a reasonable maximum (e.g. seg_max * PAGE_SIZE)
            // to prevent a malicious guest from causing unbounded allocation.
            let data_len_usize = data_len as usize;
            let mut bounce_mem = GuestMemory::allocate(data_len_usize);
            let num_pages = data_len_usize.div_ceil(guestmem::PAGE_SIZE);
            let gpns: Vec<u64> = (0..num_pages as u64).collect();

            if !is_read {
                let buf = bounce_mem.inner_buf_mut().unwrap();
                copy_regions(buf, mem, &regions, true)?;
            }

            (
                Some(bounce_mem),
                OwnedPagedRange {
                    gpns,
                    offset: 0,
                    len: data_len_usize,
                },
                true,
            )
        };

    let effective_mem = io_mem.as_ref().unwrap_or(mem);
    let range =
        PagedRange::new(io_range.offset, io_range.len, &io_range.gpns).ok_or(VIRTIO_BLK_S_IOERR)?;
    let buffers = RequestBuffers::new(effective_mem, range, is_read);

    if is_read {
        disk.read_vectored(&buffers, start_disk_sector)
            .await
            .map_err(|_| VIRTIO_BLK_S_IOERR)?;
    } else {
        disk.write_vectored(&buffers, start_disk_sector, false)
            .await
            .map_err(|_| VIRTIO_BLK_S_IOERR)?;
    }

    if bounced && is_read {
        let buf = io_mem.as_mut().unwrap().inner_buf_mut().unwrap();
        copy_regions(buf, mem, &regions, false)?;
    }

    Ok((if is_read { io_range.len as u32 } else { 0 }, bounced))
}

/// Write the status byte to the last writable byte in the descriptor chain.
fn write_status_byte(
    mem: &GuestMemory,
    work: &VirtioQueueCallbackWork,
    status: u8,
) -> Result<(), virtio::VirtioWriteError> {
    let writable_len = work.get_payload_length(true);
    if writable_len == 0 {
        return Err(virtio::VirtioWriteError::NotAllWritten(1));
    }
    work.write_at_offset(writable_len - 1, mem, &[status])
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh::rpc::RpcSend;
    use std::task::Context;
    use std::task::Waker;
    use test_with_tracing::test;
    use vmcore::vm_task::SingleDriverBackend;

    // ---- copy_regions tests ----

    #[test]
    fn copy_regions_roundtrip() {
        // End-to-end: scatter → linear → scatter should preserve data.
        let guest = GuestMemory::allocate(0x10000);
        let mut bounce = [0u8; 2048];

        let regions = vec![
            DataRegion {
                addr: 0x1000,
                len: 1000,
            },
            DataRegion {
                addr: 0x5000,
                len: 500,
            },
            DataRegion {
                addr: 0x9000,
                len: 548,
            },
        ];

        // Write distinct patterns at each guest region.
        guest.write_at(0x1000, &[0x11; 1000]).unwrap();
        guest.write_at(0x5000, &[0x22; 500]).unwrap();
        guest.write_at(0x9000, &[0x33; 548]).unwrap();

        // Scatter → linear.
        copy_regions(&mut bounce, &guest, &regions, true).unwrap();

        // Clear guest memory to prove the round-trip actually copies back.
        guest.write_at(0x1000, &[0x00; 1000]).unwrap();
        guest.write_at(0x5000, &[0x00; 500]).unwrap();
        guest.write_at(0x9000, &[0x00; 548]).unwrap();

        // Linear → scatter.
        copy_regions(&mut bounce, &guest, &regions, false).unwrap();

        // Verify data restored.
        let mut buf = [0u8; 1000];
        guest.read_at(0x1000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x11));

        let mut buf = [0u8; 500];
        guest.read_at(0x5000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x22));

        let mut buf = [0u8; 548];
        guest.read_at(0x9000, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x33));
    }

    fn image_slot(active: bool) -> VirtioBlkDevice {
        let driver = pal_async::DefaultPool::new().driver();
        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let (_requests, receiver) = mesh::channel();
        VirtioBlkDevice::new_image_slot(&source, active, receiver)
    }

    fn bind_request(
        identity: &str,
        length: u64,
        logical_block_size: u32,
    ) -> virtio_resources::blk::BindImageSlotRequest {
        let file = tempfile::tempfile().unwrap();
        file.set_len(length).unwrap();
        virtio_resources::blk::BindImageSlotRequest {
            media: file,
            identity: identity.to_owned(),
            length,
            logical_block_size,
            physical_block_size: 4096,
        }
    }

    #[test]
    fn inactive_image_slot_is_a_placeholder() {
        let device = image_slot(false);
        assert_eq!(device.traits().device_id.0, 0);
        assert_eq!(device.image_slot_state().capacity_sectors, 0);
    }

    #[test]
    fn image_slot_bind_once_is_idempotent_by_identity() {
        let device = image_slot(true);
        let state = device
            .bind_image_slot(bind_request("image-a", 4096, 512))
            .unwrap();
        assert_eq!(state.capacity_sectors, 8);
        assert_eq!(state.identity.as_deref(), Some("image-a"));

        let state = device
            .bind_image_slot(bind_request("image-a", 4096, 512))
            .unwrap();
        assert_eq!(state.capacity_sectors, 8);
        assert!(matches!(
            device.bind_image_slot(bind_request("image-b", 4096, 512)),
            Err(virtio_resources::blk::BindImageSlotError::AlreadyBound)
        ));
    }

    #[test]
    fn image_slot_rejects_geometry_mismatch() {
        let device = image_slot(true);
        assert!(matches!(
            device.bind_image_slot(bind_request("image-a", 4096, 4096)),
            Err(virtio_resources::blk::BindImageSlotError::GeometryMismatch)
        ));
    }

    #[test]
    fn image_slot_bind_reports_configuration_change() {
        let driver = pal_async::DefaultPool::new().driver();
        let source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let (requests, receiver) = mesh::channel();
        let mut device = VirtioBlkDevice::new_image_slot(&source, true, receiver);
        let response = requests.call(
            virtio_resources::blk::ImageSlotRequest::Bind,
            bind_request("image-a", 4096, 512),
        );
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(
            device.poll_config_change(&mut context),
            Poll::Ready(Ok(true))
        ));
        let state = futures::executor::block_on(response).unwrap().unwrap();
        assert_eq!(state.capacity_sectors, 8);
        assert_eq!(futures::executor::block_on(device.read_registers_u32(0)), 8);
    }
}
