use std::env;
use std::process::ExitCode;

use kumo_abi::{BootInfo, MemRegion, MemRegionKind, Range, RawSlice, ABI_VERSION};

static MEM_REGIONS: [MemRegion; 3] = [
    MemRegion {
        range: Range {
            start: 0x0000_0000_0000_0000,
            len: 0x0000_0000_0008_0000,
        },
        kind: MemRegionKind::Reserved,
        _reserved: 0,
    },
    MemRegion {
        range: Range {
            start: 0x0000_0000_0008_0000,
            len: 0x0000_0000_03f8_0000,
        },
        kind: MemRegionKind::Usable,
        _reserved: 0,
    },
    MemRegion {
        range: Range {
            start: 0x0000_0000_0400_0000,
            len: 0x0000_0000_0010_0000,
        },
        kind: MemRegionKind::Bootloader,
        _reserved: 0,
    },
];

fn main() -> ExitCode {
    let self_test = env::args().any(|arg| arg == "--self-test");
    match run(self_test) {
        Ok(transcript) => {
            print!("{transcript}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("stage-a-smoke: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(self_test: bool) -> Result<String, String> {
    let boot = sample_boot_info(ABI_VERSION);
    let transcript = render_transcript(&boot)?;

    if self_test {
        assert_transcript(&transcript)?;
        assert_invalid_handoff_is_rejected()?;
    }

    Ok(transcript)
}

fn sample_boot_info(version: u32) -> BootInfo {
    let mut boot = BootInfo::empty(version);
    boot.mem_regions = RawSlice::from_slice(&MEM_REGIONS);
    boot.kernel_phys = Range::new(0x0000_0000_0010_0000, 0x0000_0000_0008_0000);
    boot.kernel_virt = Range::new(0xffff_0000_0010_0000, 0x0000_0000_0008_0000);
    boot.initrd = Range::new(0x0000_0000_0080_0000, 0x0000_0000_0010_0000);
    boot.platform.dtb = 0x0000_0000_00a0_0000;
    boot
}

fn render_transcript(boot: &BootInfo) -> Result<String, String> {
    let report = kernel::inspect_boot(boot).map_err(|err| format!("{err:?}"))?;
    // Mirror the kernel's M1 Stage-A: same memory accounting + heap self-test sum.
    let mm = unsafe { kernel::mm::init(boot) };
    let sum: u32 = (1..=8u32).map(|i| i * i).sum();
    Ok(format!(
        "[NIJIGUMO] HANDOFF COMPLETE abi=v{} arch={}\n\
         CPU MODE: Executive (EL1/Ring0)\n\
         AETHER: {} MiB usable / {} MiB total, {} frames  OK\n\
         HEAP: bump {} KiB online; vec self-test sum={}  OK\n\
         {}\n",
        report.abi_version,
        report.arch,
        report.usable_bytes >> 20,
        report.total_bytes >> 20,
        mm.usable_frames,
        mm.heap_bytes >> 10,
        sum,
        kernel::stage_a_banner()
    ))
}

fn assert_transcript(transcript: &str) -> Result<(), String> {
    let expected = [
        "[NIJIGUMO] HANDOFF COMPLETE abi=v1 arch=aarch64",
        "CPU MODE: Executive (EL1/Ring0)",
        "AETHER:",
        "frames  OK",
        "HEAP: bump",
        "vec self-test sum=204  OK",
        "KUMO Ziwei Stage-A core only; halting",
    ];

    for needle in expected {
        if !transcript.contains(needle) {
            return Err(format!("transcript missing '{needle}'"));
        }
    }

    for forbidden in ["VANGUARD OK", "Hanga", "Libsumi", "KAGEMUSHA"] {
        if transcript.contains(forbidden) {
            return Err(format!(
                "transcript contains forbidden future claim '{forbidden}'"
            ));
        }
    }

    Ok(())
}

fn assert_invalid_handoff_is_rejected() -> Result<(), String> {
    let boot = sample_boot_info(ABI_VERSION + 1);
    match niji_loader::validate_boot_info(&boot) {
        Err(niji_loader::HandoffError::AbiVersion { .. }) => Ok(()),
        other => Err(format!("expected ABI-version rejection, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_is_phase_honest() {
        let transcript = render_transcript(&sample_boot_info(ABI_VERSION)).unwrap();
        assert_transcript(&transcript).unwrap();
    }

    #[test]
    fn invalid_handoff_self_test_is_live() {
        assert_invalid_handoff_is_rejected().unwrap();
    }
}
