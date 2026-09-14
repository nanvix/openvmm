// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmError;

pub(crate) fn advance_tsc(vp: &kvm::Processor<'_>, cycles: u64) -> Result<(), KvmError> {
    // KVM can discard sub-second IA32_TSC writes as synchronization attempts.
    // Adjusting the offset also avoids introducing read/write latency skew.
    let offset = vp.tsc_offset()?;
    vp.set_tsc_offset(offset.wrapping_add(cycles))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::advance_tsc;
    use test_with_tracing::test;

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn snapshot_tsc_downtime_advances_exact_cycles() {
        let kvm = kvm::Kvm::new().unwrap();
        for vp_count in [1, 2, 4, 8] {
            let mut partition = kvm.new_vm(kvm::VmType::Default).unwrap();
            for index in 0..vp_count {
                partition.add_vp(index).unwrap();
            }
            let frequency = partition.vp(0).tsc_frequency_hz().unwrap();
            for index in 0..vp_count {
                partition
                    .vp(index)
                    .set_msrs(&[(x86defs::X86X_MSR_TSC, frequency * 10)])
                    .unwrap();
            }
            for cycles in [frequency / 4, frequency * 2, 0, 1] {
                for index in 0..vp_count {
                    let vp = partition.vp(index);
                    let before = vp.tsc_offset().unwrap();
                    advance_tsc(&vp, cycles).unwrap();
                    assert_eq!(
                        vp.tsc_offset().unwrap(),
                        before.wrapping_add(cycles),
                        "VP {index} of {vp_count}, delta {cycles} cycles"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn snapshot_tsc_downtime_preserves_wrapping_per_vp_offsets() {
        let kvm = kvm::Kvm::new().unwrap();
        let mut partition = kvm.new_vm(kvm::VmType::Default).unwrap();
        let offsets = [u64::MAX - 10, 0, 1_000, 10_000];
        for (index, offset) in offsets.into_iter().enumerate() {
            partition.add_vp(index as u32).unwrap();
            partition.vp(index as u32).set_tsc_offset(offset).unwrap();
        }
        for (index, offset) in offsets.into_iter().enumerate() {
            let vp = partition.vp(index as u32);
            advance_tsc(&vp, 100).unwrap();
            assert_eq!(vp.tsc_offset().unwrap(), offset.wrapping_add(100));
        }
    }
}
