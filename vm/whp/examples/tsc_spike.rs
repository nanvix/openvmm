// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SPIKE (time ABI v1, not for integration): measures the live TSC offset of
//! every VP of a raw WHP partition against the host TSC, to evaluate
//! restore-time TSC synchronization primitives without a guest kernel.
//!
//! Each VP runs a real-mode loop that answers a host ping through a shared
//! cache line with `lfence; rdtsc`. The host brackets each ping with its own
//! TSC, so `guest - (h0 + h1) / 2` estimates the VP's offset with an error of
//! at most half the round trip. The minimum-round-trip samples bound the
//! cross-VP skew to well under a microsecond on bare metal.
//!
//! Usage: `tsc_spike <scenario> [--vps N] [--samples S] [--host-lp L]
//! [--vp-lps a,b,..] [--delay-ms X] [--duration-s D] [--interval-ms I]`.

#[cfg(all(windows, target_arch = "x86_64"))]
#[expect(unsafe_code)]
#[expect(clippy::undocumented_unsafe_blocks)]
mod imp {
    use std::arch::x86_64::_mm_lfence;
    use std::arch::x86_64::_rdtsc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;
    use windows_sys::Win32::System::Memory::MEM_COMMIT;
    use windows_sys::Win32::System::Memory::MEM_RESERVE;
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    use windows_sys::Win32::System::Memory::VirtualAlloc;
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    use windows_sys::Win32::System::Threading::SetThreadAffinityMask;
    use windows_sys::Win32::System::Threading::SetThreadPriority;
    use windows_sys::Win32::System::Threading::THREAD_PRIORITY_HIGHEST;

    const MAP_RWX: whp::abi::WHV_MAP_GPA_RANGE_FLAGS = whp::abi::WHV_MAP_GPA_RANGE_FLAGS(
        whp::abi::WHvMapGpaRangeFlagRead.0
            | whp::abi::WHvMapGpaRangeFlagWrite.0
            | whp::abi::WHvMapGpaRangeFlagExecute.0,
    );
    const MEM_SIZE: usize = 0x10000;
    const CODE_GPA: u64 = 0x1000;
    const SLOT_GPA: u64 = 0x2000;
    const SLOT_SIZE: usize = 64;
    const STOP: u32 = u32::MAX;

    /// Real-mode responder. BX points at this VP's slot:
    /// +0 request sequence (host), +8 acknowledged sequence (guest),
    /// +16 guest TSC.
    const CODE: &[u8] = &[
        0x66, 0x8b, 0x0f, // 0: mov ecx, [bx]
        0x66, 0x3b, 0x4f, 0x08, // 3: cmp ecx, [bx+8]
        0x74, 0xf7, // 7: je 0
        0x66, 0x83, 0xf9, 0xff, // 9: cmp ecx, -1
        0x74, 0x13, // 13: je 34
        0x0f, 0xae, 0xe8, // 15: lfence
        0x0f, 0x31, // 18: rdtsc
        0x66, 0x89, 0x47, 0x10, // 20: mov [bx+16], eax
        0x66, 0x89, 0x57, 0x14, // 24: mov [bx+20], edx
        0x66, 0x89, 0x4f, 0x08, // 28: mov [bx+8], ecx
        0xeb, 0xde, // 32: jmp 0
        0xe6, 0x80, // 34: out 0x80, al
        0xeb, 0xfc, // 36: jmp 34
    ];

    #[repr(C, align(64))]
    struct Slot {
        request: AtomicU32,
        _pad0: u32,
        ack: AtomicU32,
        _pad1: u32,
        tsc: AtomicU64,
    }

    #[inline(always)]
    fn rdtsc() -> u64 {
        unsafe {
            _mm_lfence();
            let t = _rdtsc();
            _mm_lfence();
            t
        }
    }

    fn pin(lp: Option<usize>) {
        unsafe {
            let thread = GetCurrentThread();
            if let Some(lp) = lp {
                assert!(SetThreadAffinityMask(thread, 1usize << lp) != 0, "pin {lp}");
            }
            SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST);
        }
    }

    struct Options {
        vps: u32,
        samples: usize,
        host_lp: Option<usize>,
        vp_lps: Vec<usize>,
        delay_ms: u64,
        duration_s: u64,
        interval_ms: u64,
        offset: i64,
    }

    fn parse(args: &[String]) -> Options {
        let mut o = Options {
            vps: 8,
            samples: 2000,
            host_lp: Some(0),
            vp_lps: Vec::new(),
            delay_ms: 0,
            duration_s: 10,
            interval_ms: 1000,
            offset: 1_000_000,
        };
        let mut i = 0;
        while i < args.len() {
            let v = args.get(i + 1).cloned().unwrap_or_default();
            match args[i].as_str() {
                "--vps" => o.vps = v.parse().unwrap(),
                "--samples" => o.samples = v.parse().unwrap(),
                "--host-lp" => {
                    o.host_lp = if v == "none" {
                        None
                    } else {
                        Some(v.parse().unwrap())
                    }
                }
                "--vp-lps" => o.vp_lps = v.split(',').map(|x| x.parse().unwrap()).collect(),
                "--delay-ms" => o.delay_ms = v.parse().unwrap(),
                "--duration-s" => o.duration_s = v.parse().unwrap(),
                "--interval-ms" => o.interval_ms = v.parse().unwrap(),
                "--offset" => o.offset = v.parse().unwrap(),
                other => panic!("unknown option {other}"),
            }
            i += 2;
        }
        o
    }

    struct Vm {
        partition: Arc<whp::Partition>,
        mem: *mut u8,
        vps: u32,
        threads: Vec<std::thread::JoinHandle<u32>>,
    }

    impl Vm {
        fn new(vps: u32) -> Self {
            let mut config = whp::PartitionConfig::new().unwrap();
            config
                .set_property(whp::PartitionProperty::ProcessorCount(vps))
                .unwrap()
                .set_property(whp::PartitionProperty::LocalApicEmulationMode(
                    whp::abi::WHvX64LocalApicEmulationModeXApic,
                ))
                .unwrap();
            let partition = Arc::new(config.create().unwrap());
            for vp in 0..vps {
                partition.create_vp(vp).create().unwrap();
            }
            let mem = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    MEM_SIZE,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_READWRITE,
                )
                .cast::<u8>()
            };
            assert!(!mem.is_null());
            unsafe {
                std::slice::from_raw_parts_mut(mem.add(CODE_GPA as usize), CODE.len())
                    .copy_from_slice(CODE);
                partition
                    .map_range(None, mem, MEM_SIZE, 0, MAP_RWX)
                    .unwrap();
            }
            for vp in 0..vps {
                let cs = whp::abi::WHV_X64_SEGMENT_REGISTER {
                    Base: 0,
                    Limit: 0xffff,
                    Selector: 0,
                    Attributes: 0x9b,
                };
                whp::set_registers!(
                    partition.vp(vp),
                    [
                        (whp::RegisterSegment::Cs, cs),
                        (whp::Register64::Rip, CODE_GPA),
                        (
                            whp::Register64::Rbx,
                            SLOT_GPA + vp as u64 * SLOT_SIZE as u64
                        ),
                        (whp::Register64::Rflags, 2),
                        // APs start in wait-for-SIPI with the hypervisor APIC.
                        (whp::Register64::InternalActivityState, 0),
                    ]
                )
                .unwrap();
            }
            Self {
                partition,
                mem,
                vps,
                threads: Vec::new(),
            }
        }

        fn slot(&self, vp: u32) -> &Slot {
            unsafe {
                &*(self
                    .mem
                    .add(SLOT_GPA as usize + vp as usize * SLOT_SIZE)
                    .cast::<Slot>())
            }
        }

        fn start(&mut self, vp: u32, lp: Option<usize>) {
            let partition = self.partition.clone();
            self.threads.push(std::thread::spawn(move || {
                pin(lp);
                let processor = partition.vp(vp);
                let mut runner = processor.runner();
                let mut exits = 0;
                loop {
                    let exit = runner.run().unwrap();
                    exits += 1;
                    match exit.reason {
                        whp::ExitReason::IoPortAccess(_) | whp::ExitReason::Canceled => break,
                        whp::ExitReason::None => {}
                        reason => panic!("vp {vp}: unexpected exit {reason:?}"),
                    }
                }
                exits
            }));
        }

        fn start_all(&mut self, o: &Options) {
            for vp in 0..self.vps {
                let lp = o.vp_lps.get(vp as usize).copied();
                self.start(vp, lp);
            }
        }

        fn stop(&mut self) {
            for vp in 0..self.vps {
                self.slot(vp).request.store(STOP, Ordering::Release);
            }
            for thread in self.threads.drain(..) {
                thread.join().unwrap();
            }
            // Re-arm the responders for a later start.
            for vp in 0..self.vps {
                let slot = self.slot(vp);
                slot.request.store(0, Ordering::Relaxed);
                slot.ack.store(0, Ordering::Relaxed);
                self.partition
                    .vp(vp)
                    .set_register(whp::Register64::Rip, CODE_GPA)
                    .unwrap();
            }
        }

        fn tsc(&self, vp: u32) -> u64 {
            self.partition
                .vp(vp)
                .get_register(whp::Register64::Tsc)
                .unwrap()
        }

        fn set_tsc(&self, vp: u32, value: u64) {
            self.partition
                .vp(vp)
                .set_register(whp::Register64::Tsc, value)
                .unwrap();
        }
    }

    /// One VP's offset against the host TSC.
    #[derive(Clone, Copy)]
    struct Offset {
        offset: i128,
        rtt: u64,
    }

    fn ping(vm: &Vm, vp: u32, seq: u32) -> Option<(u64, u64, u64)> {
        let slot = vm.slot(vp);
        let h0 = rdtsc();
        slot.request.store(seq, Ordering::Release);
        let deadline = h0 + 20_000_000_000;
        loop {
            if slot.ack.load(Ordering::Acquire) == seq {
                break;
            }
            if rdtsc() > deadline {
                return None;
            }
            std::hint::spin_loop();
        }
        let h1 = rdtsc();
        Some((h0, h1, slot.tsc.load(Ordering::Acquire)))
    }

    /// Measures every VP's offset against the host TSC, keeping the sample
    /// with the smallest round trip.
    fn measure(vm: &Vm, samples: usize, seq: &mut u32) -> Vec<Offset> {
        let mut best = vec![
            Offset {
                offset: 0,
                rtt: u64::MAX,
            };
            vm.vps as usize
        ];
        for _ in 0..samples {
            for vp in 0..vm.vps {
                *seq = seq.wrapping_add(1);
                if *seq == STOP || *seq == 0 {
                    *seq = 1;
                }
                let Some((h0, h1, g)) = ping(vm, vp, *seq) else {
                    panic!("vp {vp} did not answer");
                };
                let rtt = h1 - h0;
                if rtt < best[vp as usize].rtt {
                    best[vp as usize] = Offset {
                        offset: g as i128 - (h0 as i128 + h1 as i128) / 2,
                        rtt,
                    };
                }
            }
        }
        best
    }

    fn ns(cycles: i128, hz: u64) -> f64 {
        cycles as f64 * 1e9 / hz as f64
    }

    fn report(label: &str, offsets: &[Offset], hz: u64) {
        let base = offsets[0].offset;
        let mut max_skew = 0f64;
        let mut max_unc = 0f64;
        for (vp, o) in offsets.iter().enumerate() {
            let skew = ns(o.offset - base, hz);
            let unc = ns(o.rtt as i128, hz) / 2.0;
            max_skew = max_skew.max(skew.abs());
            max_unc = max_unc.max(unc);
            println!(
                "RESULT {label} vp={vp} offset_cycles={} skew_vs_vp0_ns={skew:.1} unc_ns={unc:.1}",
                o.offset
            );
        }
        println!("SKEW {label} max_abs_ns={max_skew:.1} max_unc_ns={max_unc:.1}");
    }

    fn readback(label: &str, vm: &Vm, hz: u64) {
        // Host-bracketed register reads of stopped VPs.
        let mut line = format!("READBACK {label}");
        let mut base = None;
        for vp in 0..vm.vps {
            let mut best = (u64::MAX, 0i128);
            for _ in 0..200 {
                let h0 = rdtsc();
                let g = vm.tsc(vp);
                let h1 = rdtsc();
                if h1 - h0 < best.0 {
                    best = (h1 - h0, g as i128 - (h0 as i128 + h1 as i128) / 2);
                }
            }
            let base = *base.get_or_insert(best.1);
            line += &format!(
                " vp{vp}={:.1}ns(rtt={:.1}ns)",
                ns(best.1 - base, hz),
                ns(best.0 as i128, hz)
            );
        }
        println!("{line}");
    }

    fn frozen_values(label: &str, vm: &Vm) {
        let values: Vec<u64> = (0..vm.vps).map(|vp| vm.tsc(vp)).collect();
        println!("FROZEN {label} {values:?}");
    }

    fn host_skew(o: &Options, hz: u64) {
        // Cross-LP skew of the host's own TSC: ping-pong between a reference
        // thread and a responder pinned to each LP.
        let lps = std::thread::available_parallelism().unwrap().get();
        let reference = o.host_lp.unwrap_or(0);
        let mut worst = 0f64;
        for lp in 0..lps {
            if lp == reference {
                continue;
            }
            let slot = Arc::new(Slot {
                request: AtomicU32::new(0),
                _pad0: 0,
                ack: AtomicU32::new(0),
                _pad1: 0,
                tsc: AtomicU64::new(0),
            });
            let responder = {
                let slot = slot.clone();
                std::thread::spawn(move || {
                    pin(Some(lp));
                    loop {
                        let request = slot.request.load(Ordering::Acquire);
                        if request == slot.ack.load(Ordering::Relaxed) {
                            std::hint::spin_loop();
                            continue;
                        }
                        if request == STOP {
                            break;
                        }
                        slot.tsc.store(rdtsc(), Ordering::Relaxed);
                        slot.ack.store(request, Ordering::Release);
                    }
                })
            };
            pin(Some(reference));
            let mut best = (u64::MAX, 0i128);
            for seq in 1..=o.samples as u32 {
                let h0 = rdtsc();
                slot.request.store(seq, Ordering::Release);
                while slot.ack.load(Ordering::Acquire) != seq {
                    std::hint::spin_loop();
                }
                let h1 = rdtsc();
                let g = slot.tsc.load(Ordering::Relaxed);
                if h1 - h0 < best.0 {
                    best = (h1 - h0, g as i128 - (h0 as i128 + h1 as i128) / 2);
                }
            }
            slot.request.store(STOP, Ordering::Release);
            responder.join().unwrap();
            let offset = ns(best.1, hz);
            worst = worst.max(offset.abs());
            println!(
                "HOST lp={lp} vs lp={reference} offset_ns={offset:.1} unc_ns={:.1}",
                ns(best.0 as i128, hz) / 2.0
            );
        }
        println!("HOSTSKEW max_abs_ns={worst:.1}");
    }

    /// Times each partition property that the time-ABI identity sets, to
    /// attribute its partition-creation cost.
    fn props() {
        fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
            let start = Instant::now();
            let r = f();
            println!("PROP {label} us={:.1}", start.elapsed().as_secs_f64() * 1e6);
            r
        }
        let topology_leaves: [u32; 13] = [
            0,
            1,
            4,
            6,
            7,
            0xa,
            0xb,
            0x15,
            0x16,
            0x1f,
            0x8000_0007,
            0x8000_0008,
            0x8000_001e,
        ];
        for round in 0..3 {
            println!("ROUND {round}");
            for exit_list_len in [8usize, 13, 13 + 6, 13 + 256] {
                let mut config = timed("PartitionConfig::new", || {
                    whp::PartitionConfig::new().unwrap()
                });
                config
                    .set_property(whp::PartitionProperty::ProcessorCount(1))
                    .unwrap();
                let mut list: Vec<u32> = topology_leaves.to_vec();
                list.extend(0x4000_0000..=0x4000_00ffu32);
                list.truncate(exit_list_len);
                timed(&format!("ExtendedVmExits"), || {
                    config
                        .set_property(whp::PartitionProperty::ExtendedVmExits(
                            whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit
                                | whp::abi::WHV_EXTENDED_VM_EXITS::X64MsrExit,
                        ))
                        .map(drop)
                })
                .unwrap();
                timed(&format!("CpuidExitList len={exit_list_len}"), || {
                    config
                        .set_property(whp::PartitionProperty::CpuidExitList(&list))
                        .map(drop)
                })
                .unwrap();
                let features = whp::capabilities::processor_features().unwrap();
                let mut cleared = features;
                cleared.bank1 &= !(whp::abi::WHV_PROCESSOR_FEATURES1::TscDeadlineTmrSupport
                    | whp::abi::WHV_PROCESSOR_FEATURES1::TscAdjustSupport
                    | whp::abi::WHV_PROCESSOR_FEATURES1::ACountMCountSupport);
                timed("ProcessorFeatures(banks)", || {
                    config
                        .set_property(whp::PartitionProperty::ProcessorFeatures(cleared))
                        .map(drop)
                })
                .unwrap();
                timed("X64MsrExitBitmap", || {
                    config
                        .set_property(whp::PartitionProperty::X64MsrExitBitmap(
                            whp::abi::WHV_X64_MSR_EXIT_BITMAP::UnhandledMsrs,
                        ))
                        .map(drop)
                })
                .unwrap();
                let r = timed("InterruptClockFrequency(200MHz) [fails]", || {
                    config
                        .set_property(whp::PartitionProperty::InterruptClockFrequency(200_000_000))
                        .map(drop)
                });
                println!("PROP InterruptClockFrequency result={r:?}");
                let r = timed("ProcessorClockFrequency(1GHz)", || {
                    config
                        .set_property(whp::PartitionProperty::ProcessorClockFrequency(
                            1_000_000_000,
                        ))
                        .map(drop)
                });
                println!("PROP ProcessorClockFrequency result={r:?}");
                let partition = timed("WHvSetupPartition", || config.create().unwrap());
                timed("create_vp", || partition.create_vp(0).create().unwrap());
                timed("delete partition", || drop(partition));
            }
        }
    }

    /// Writes per-VP TSC values while time runs, like the generic restore
    /// path, then aligns them with partition time suspended.
    fn restore_like(vm: &Vm) -> u64 {
        let base = vm.tsc(0) + 50_000_000_000;
        for vp in 0..vm.vps {
            vm.set_tsc(vp, base + vp as u64 * 1_000);
        }
        vm.partition.suspend_time().unwrap();
        let bsp = vm.tsc(0);
        for vp in 1..vm.vps {
            vm.set_tsc(vp, bsp);
        }
        bsp
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let scenario = args.first().cloned().unwrap_or_else(|| "cold".into());
        if scenario == "props" {
            props();
            return;
        }
        let o = parse(&args[1.min(args.len())..]);
        let hz = whp::capabilities::processor_clock_frequency().unwrap();
        println!(
            "INFO scenario={scenario} vps={} samples={} host_lp={:?} vp_lps={:?} tsc_hz={hz} lps={}",
            o.vps,
            o.samples,
            o.host_lp,
            o.vp_lps,
            std::thread::available_parallelism().unwrap()
        );
        pin(o.host_lp);
        let mut seq = 0;
        match scenario.as_str() {
            "host" => host_skew(&o, hz),
            "cold" => {
                let mut vm = Vm::new(o.vps);
                frozen_values("created", &vm);
                std::thread::sleep(Duration::from_millis(20));
                frozen_values("created+20ms", &vm);
                vm.start_all(&o);
                report("cold", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
                readback("cold-stopped", &vm, hz);
            }
            "write-running" => {
                // Run once so partition time is live, then write equal TSCs
                // without suspending time.
                let mut vm = Vm::new(o.vps);
                vm.start_all(&o);
                report(
                    "write-running-before",
                    &measure(&vm, o.samples, &mut seq),
                    hz,
                );
                vm.stop();
                let base = vm.tsc(0) + 50_000_000_000;
                for vp in 0..o.vps {
                    vm.set_tsc(vp, base);
                }
                readback("write-running", &vm, hz);
                vm.start_all(&o);
                report("write-running", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
            }
            "aged-implicit" | "aged-explicit" => {
                // Partition time is already running when the restore-like
                // sequence starts.
                let explicit = scenario == "aged-explicit";
                let mut vm = Vm::new(o.vps);
                vm.start_all(&o);
                report("aged-before", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
                std::thread::sleep(Duration::from_millis(50));
                restore_like(&vm);
                frozen_values("aged-suspended", &vm);
                if explicit {
                    vm.partition.resume_time().unwrap();
                    readback("aged-resumed", &vm, hz);
                }
                vm.start(0, o.vp_lps.first().copied());
                std::thread::sleep(Duration::from_millis(o.delay_ms));
                for vp in 1..o.vps {
                    vm.start(vp, o.vp_lps.get(vp as usize).copied());
                }
                report(&scenario, &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
            }
            "restore-implicit" | "restore-explicit" => {
                let explicit = scenario == "restore-explicit";
                let mut vm = Vm::new(o.vps);
                restore_like(&vm);
                frozen_values("suspended", &vm);
                std::thread::sleep(Duration::from_millis(20));
                frozen_values("suspended+20ms", &vm);
                if explicit {
                    vm.partition.resume_time().unwrap();
                    readback("resumed", &vm, hz);
                }
                let start = Instant::now();
                vm.start(0, o.vp_lps.first().copied());
                std::thread::sleep(Duration::from_millis(o.delay_ms));
                for vp in 1..o.vps {
                    vm.start(vp, o.vp_lps.get(vp as usize).copied());
                }
                report(&scenario, &measure(&vm, o.samples, &mut seq), hz);
                println!(
                    "INFO first_measure_after_ms={}",
                    start.elapsed().as_millis()
                );
                vm.stop();
                readback(&format!("{scenario}-stopped"), &vm, hz);
            }
            "rerun" => {
                // Does stopping and re-running VPs change their offsets?
                let mut vm = Vm::new(o.vps);
                restore_like(&vm);
                vm.partition.resume_time().unwrap();
                vm.start_all(&o);
                report("rerun-1", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
                std::thread::sleep(Duration::from_millis(o.delay_ms));
                vm.start_all(&o);
                report("rerun-2", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
            }
            "resuspend" => {
                // A second suspend/align/resume pass after the VPs ran.
                let mut vm = Vm::new(o.vps);
                restore_like(&vm);
                vm.start_all(&o);
                report("resuspend-before", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
                vm.partition.suspend_time().unwrap();
                let bsp = vm.tsc(0);
                for vp in 1..o.vps {
                    vm.set_tsc(vp, bsp);
                }
                vm.partition.resume_time().unwrap();
                vm.start_all(&o);
                report("resuspend-after", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
            }
            "virtual-offset" => {
                let mut vm = Vm::new(o.vps);
                for vp in 0..o.vps {
                    let value = vm
                        .partition
                        .vp(vp)
                        .get_register(whp::Register64::TscVirtualOffset);
                    println!("VOFFSET vp={vp} initial={value:?}");
                }
                vm.start_all(&o);
                report("voffset-before", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
                for value in [1u64, 1000, (-1000i64) as u64] {
                    let r = vm
                        .partition
                        .vp(1)
                        .set_register(whp::Register64::TscVirtualOffset, value);
                    println!("VOFFSET probe vp=1 value={value:#x} result={r:?}");
                }
                vm.partition.suspend_time().unwrap();
                let r = vm
                    .partition
                    .vp(1)
                    .set_register(whp::Register64::TscVirtualOffset, 1000);
                println!("VOFFSET suspended vp=1 value=1000 result={r:?}");
                vm.partition.resume_time().unwrap();
                let r = vm
                    .partition
                    .vp(1)
                    .set_register(whp::Register64::TscVirtualOffset, o.offset as u64);
                println!("VOFFSET set vp=1 value={} result={r:?}", o.offset);
                for vp in 0..o.vps {
                    let value = vm
                        .partition
                        .vp(vp)
                        .get_register(whp::Register64::TscVirtualOffset);
                    println!("VOFFSET vp={vp} after={value:?}");
                }
                vm.start_all(&o);
                report("voffset-after", &measure(&vm, o.samples, &mut seq), hz);
                vm.stop();
            }
            "rate" => {
                // Long-run rate of a VP's live TSC against host UTC and host
                // monotonic time: least-squares slope, deviation from the
                // declared rate, and residuals.
                let mut vm = Vm::new(1);
                vm.start_all(&o);
                let start = Instant::now();
                let mut points: Vec<(f64, f64, f64)> = Vec::new();
                let mut next = Duration::ZERO;
                while start.elapsed() <= Duration::from_secs(o.duration_s) {
                    if start.elapsed() >= next {
                        let mut best = (u64::MAX, 0u64, 0f64, 0f64);
                        for _ in 0..200 {
                            seq = seq.wrapping_add(1).max(1);
                            if seq == STOP {
                                seq = 1;
                            }
                            let mono = start.elapsed().as_secs_f64();
                            let utc = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_secs_f64();
                            let (h0, h1, g) = ping(&vm, 0, seq).expect("vp answers");
                            if h1 - h0 < best.0 {
                                best = (h1 - h0, g, utc, mono);
                            }
                        }
                        points.push((best.2, best.3, best.1 as f64));
                        next += Duration::from_millis(o.interval_ms);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                vm.stop();
                fn fit(points: &[(f64, f64)]) -> (f64, f64) {
                    let n = points.len() as f64;
                    let (sx, sy) = points
                        .iter()
                        .fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
                    let (mx, my) = (sx / n, sy / n);
                    let (num, den) = points.iter().fold((0.0, 0.0), |(a, b), (x, y)| {
                        (a + (x - mx) * (y - my), b + (x - mx) * (x - mx))
                    });
                    let slope = num / den;
                    let max_residual_s = points
                        .iter()
                        .map(|(x, y)| ((y - my) - slope * (x - mx)).abs() / slope)
                        .fold(0.0, f64::max);
                    (slope, max_residual_s)
                }
                let t0 = points[0];
                let utc: Vec<(f64, f64)> =
                    points.iter().map(|p| (p.0 - t0.0, p.2 - t0.2)).collect();
                let mono: Vec<(f64, f64)> =
                    points.iter().map(|p| (p.1 - t0.1, p.2 - t0.2)).collect();
                for (label, series) in [("utc", &utc), ("monotonic", &mono)] {
                    let (slope, residual) = fit(series);
                    println!(
                        "RATE vs={label} samples={} span_s={:.1} tsc_hz={slope:.1} declared_hz={hz} deviation_ppm={:.3} max_residual_us={:.2}",
                        series.len(),
                        series.last().unwrap().0,
                        (slope - hz as f64) / hz as f64 * 1e6,
                        residual * 1e6
                    );
                }
            }
            "drift" => {
                let mut vm = Vm::new(o.vps);
                restore_like(&vm);
                if o.delay_ms == 0 {
                    vm.partition.resume_time().unwrap();
                }
                vm.start_all(&o);
                let start = Instant::now();
                let mut next = Duration::ZERO;
                while start.elapsed() < Duration::from_secs(o.duration_s) {
                    if start.elapsed() >= next {
                        let label = format!("drift-t{}ms", start.elapsed().as_millis());
                        report(&label, &measure(&vm, o.samples, &mut seq), hz);
                        next += Duration::from_millis(o.interval_ms);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                vm.stop();
            }
            other => panic!("unknown scenario {other}"),
        }
    }
}

fn main() {
    #[cfg(all(windows, target_arch = "x86_64"))]
    imp::main();
}
