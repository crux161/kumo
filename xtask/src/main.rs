use std::env;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use imager::{DtbSummary, HardwareTarget, ImageArch, ImagePlan};
use kumo_abi::initrd::{
    DRV_SERIAL_PATH, INITRD_ENTRY_LEN, INITRD_HEADER_LEN, INITRD_MAGIC, INITRD_PATH_MAX,
    INITRD_VERSION, PERSONA_LINUX_HELLO_PATH, SORA_INIT_PATH, SVC_HEALTH_PATH,
};

const FAT32_IMG_PATH: &str = "bin/fat32.img";

/// Staged kernel image + initrd locations on the ESP (must match the paths
/// `niji-uefi` opens at runtime).
const KERNEL_ESP_PATH: &str = "EFI/KUMO/kernel/kumo-kernel.elf";
const INITRD_ESP_PATH: &str = "EFI/KUMO/initrd.img";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    const ALL: [Self; 2] = [Self::Aarch64, Self::X86_64];

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            "x86_64" | "amd64" => Ok(Self::X86_64),
            other => Err(format!("unknown arch '{other}'")),
        }
    }

    fn kernel_feature(self) -> &'static str {
        match self {
            Self::Aarch64 => "arch_aarch64",
            Self::X86_64 => "arch_x86_64",
        }
    }

    fn image_arch(self) -> ImageArch {
        match self {
            Self::Aarch64 => ImageArch::Aarch64,
            Self::X86_64 => ImageArch::X86_64,
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aarch64 => f.write_str("aarch64"),
            Self::X86_64 => f.write_str("x86_64"),
        }
    }
}

#[derive(Debug)]
struct Args {
    command: String,
    arch: Arch,
    hardware: Option<HardwareTarget>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let root = workspace_root()?;
    let hardware = args
        .hardware
        .unwrap_or_else(|| HardwareTarget::default_for_arch(args.arch.image_arch()));
    if hardware.profile().arch != args.arch.image_arch() {
        return Err(format!(
            "hardware target '{}' is {}, but --arch selected {}",
            hardware,
            hardware.profile().arch,
            args.arch
        ));
    }

    match args.command.as_str() {
        "build" => build(&root, args.arch),
        "test" => test(&root, args.arch),
        "boot-files" => {
            let boot = build_arm64_qemu_boot_files(&root)?;
            verify_arm64_qemu_boot_files(&boot)?;
            println!("{}", boot.image.display());
            Ok(())
        }
        "qemu-smoke" => {
            let boot = build_arm64_qemu_boot_files(&root)?;
            verify_arm64_qemu_boot_files(&boot)?;
            run_qemu_smoke_if_available(&boot)
        }
        "image" => image(&root, args.arch, hardware),
        "product" => {
            let products = build_products(&root)?;
            println!("{}", products.host_stage.display());
            println!("{}", products.arm64_qemu.image.display());
            Ok(())
        }
        "run" => run_smoke(&root, args.arch),
        "preflight" => preflight(&root),
        "help" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_args() -> Result<Args, String> {
    let mut iter = env::args().skip(1);
    let command = iter.next().unwrap_or_else(|| "help".to_owned());
    let mut arch = Arch::Aarch64;
    let mut hardware = None;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--arch" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--arch requires a value".to_owned())?;
                arch = Arch::parse(&value)?;
            }
            "--hardware" | "--board" => {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                hardware = Some(parse_hardware_target(&value)?);
            }
            "-h" | "--help" => {
                return Ok(Args {
                    command: "help".to_owned(),
                    arch,
                    hardware,
                });
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    Ok(Args {
        command,
        arch,
        hardware,
    })
}

fn parse_hardware_target(value: &str) -> Result<HardwareTarget, String> {
    match value {
        "x13s"
        | "thinkpad-x13s"
        | "thinkpad-x13s-gen1"
        | "lenovo-thinkpad-x13s"
        | "sc8280xp-lenovo-thinkpad-x13s" => Ok(HardwareTarget::ThinkPadX13sGen1),
        "qemu" | "qemu-virt" | "qemu-virt-aarch64" => Ok(HardwareTarget::QemuVirtAarch64),
        "rpi5" | "pi5" | "raspberry-pi-5" | "raspberrypi5" | "raspberry-pi5" => {
            Ok(HardwareTarget::RaspberryPi5)
        }
        "generic-x86_64" | "generic-uefi-x86_64" | "x86_64" => {
            Ok(HardwareTarget::GenericUefiX86_64)
        }
        other => Err(format!("unknown hardware target '{other}'")),
    }
}

fn workspace_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not find workspace root".to_owned())
}

fn build(root: &Path, arch: Arch) -> Result<(), String> {
    run_cargo(root, &["check", "--workspace", "--exclude", "xtask"])?;
    for backend in Arch::ALL {
        check_kernel_backend(root, backend)?;
    }

    println!(
        "KUMO build guardrail green: checked both HAL backends; selected image arch is {arch}"
    );
    let products = build_products(root)?;
    println!("KUMO host product: {}", products.host_stage.display());
    println!(
        "KUMO arm64 boot image: {}",
        products.arm64_qemu.image.display()
    );
    Ok(())
}

fn test(root: &Path, arch: Arch) -> Result<(), String> {
    run_cargo(root, &["test", "--workspace", "--exclude", "xtask"])?;
    for backend in Arch::ALL {
        test_kernel_backend(root, backend)?;
    }

    println!("KUMO test guardrail green: tested both HAL backends; selected image arch is {arch}");
    let products = build_products(root)?;
    run_product_self_test(&products.host_stage)?;
    verify_arm64_qemu_boot_files(&products.arm64_qemu)?;
    run_qemu_smoke_if_available(&products.arm64_qemu)?;
    Ok(())
}

fn image(root: &Path, arch: Arch, hardware: HardwareTarget) -> Result<(), String> {
    let out_dir = root.join("build/images");
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let plan = ImagePlan::new("", hardware);
    let bootloader = stage_uefi_bootloader(root, &out_dir, &plan)?;
    let staged = stage_image_assets(root, &out_dir, &plan)?;
    let kernel = stage_kernel(root, &out_dir, &plan)?;
    let initrd = stage_initrd(&out_dir, &plan)?;
    let mut manifest = image_manifest(&plan, bootloader.as_ref(), &staged);
    if let Some(asset) = &kernel {
        manifest.push_str(&format!(
            "kernel_source_path={}\n",
            asset.source_path.display()
        ));
        manifest.push_str(&format!("kernel_esp_path={}\n", asset.esp_path.display()));
        manifest.push_str(&format!(
            "kernel_staged_path={}\n",
            asset.staged_path.display()
        ));
        manifest.push_str(&format!("kernel_size={}\n", asset.byte_len));
        manifest.push_str(&format!("kernel_entry=0x{:016x}\n", asset.entry));
        manifest.push_str(&format!(
            "kernel_fingerprint=fnv1a64:{:016x}\n",
            asset.fingerprint
        ));
    }
    if let Some(asset) = &initrd {
        manifest.push_str(&format!("initrd_esp_path={}\n", asset.esp_path.display()));
        manifest.push_str(&format!(
            "initrd_staged_path={}\n",
            asset.staged_path.display()
        ));
        manifest.push_str(&format!("initrd_size={}\n", asset.byte_len));
        manifest.push_str(&format!(
            "initrd_fingerprint=fnv1a64:{:016x}\n",
            asset.fingerprint
        ));
    }
    let manifest_path = out_dir.join("kumo-image-plan.txt");
    let hardware_manifest_path = out_dir.join(format!("kumo-image-plan-{hardware}.txt"));
    fs::write(&manifest_path, manifest)
        .map_err(|err| format!("write {}: {err}", manifest_path.display()))?;
    fs::copy(&manifest_path, &hardware_manifest_path).map_err(|err| {
        format!(
            "copy {} to {}: {err}",
            manifest_path.display(),
            hardware_manifest_path.display()
        )
    })?;

    println!("KUMO image plan hardware target: {hardware} ({arch})");
    println!("{}", manifest_path.display());
    println!("{}", hardware_manifest_path.display());
    if let Some(asset) = &bootloader {
        println!("{}", asset.staged_path.display());
    }
    for asset in &staged {
        println!("{}", asset.staged_path.display());
    }
    if let Some(asset) = &kernel {
        println!("{}", asset.staged_path.display());
    }
    if let Some(asset) = &initrd {
        println!("{}", asset.staged_path.display());
    }
    Ok(())
}

#[derive(Debug)]
struct StagedBootloader {
    source_path: PathBuf,
    esp_path: PathBuf,
    staged_path: PathBuf,
    byte_len: u64,
    fingerprint: u64,
}

#[derive(Debug)]
struct StagedAsset {
    source_path: PathBuf,
    esp_path: PathBuf,
    staged_path: PathBuf,
    byte_len: u64,
    fingerprint: u64,
    dtb_model: Option<String>,
}

fn stage_uefi_bootloader(
    root: &Path,
    out_dir: &Path,
    plan: &ImagePlan,
) -> Result<Option<StagedBootloader>, String> {
    let (target, src_name) = match plan.arch {
        ImageArch::Aarch64 => ("aarch64-unknown-uefi", "niji-uefi.efi"),
        ImageArch::X86_64 => ("x86_64-unknown-uefi", "niji-uefi.efi"),
    };

    run_cargo(
        root,
        &[
            "build",
            "-p",
            "niji-uefi",
            "--bin",
            "niji-uefi",
            "--target",
            target,
        ],
    )?;

    let source_path = root
        .join("target")
        .join(target)
        .join("debug")
        .join(src_name);
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    // Validate EFI application for the matching arch.
    match plan.arch {
        ImageArch::Aarch64 => validate_aarch64_efi_application(&bytes)
            .map_err(|err| format!("validate {source_path:?} as AA64 EFI app: {err}"))?,
        ImageArch::X86_64 => validate_x86_64_efi_application(&bytes)
            .map_err(|err| format!("validate {source_path:?} as x64 EFI app: {err}"))?,
    }

    let staged_path = out_dir
        .join(plan.hardware.to_string())
        .join(&plan.esp_boot_path);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&staged_path, &bytes)
        .map_err(|err| format!("write {}: {err}", staged_path.display()))?;

    Ok(Some(StagedBootloader {
        source_path,
        esp_path: plan.esp_boot_path.clone(),
        staged_path,
        byte_len: bytes.len() as u64,
        fingerprint: fnv1a64(&bytes),
    }))
}

fn stage_image_assets(
    root: &Path,
    out_dir: &Path,
    plan: &ImagePlan,
) -> Result<Vec<StagedAsset>, String> {
    let mut staged = Vec::new();

    let Some(dtb_source_path) = &plan.dtb_source_path else {
        return Ok(staged);
    };
    let Some(dtb_esp_path) = &plan.dtb_path else {
        return Err(format!(
            "hardware target '{}' has a DTB source but no ESP DTB path",
            plan.hardware
        ));
    };

    let source_path = root.join(dtb_source_path);
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    let summary = DtbSummary::parse(&bytes)
        .map_err(|err| format!("validate {} as DTB: {err}", source_path.display()))?;
    if !summary.has_compatibles(&plan.dtb_compatibles) {
        return Err(format!(
            "{} root compatibles {:?} do not satisfy {:?}",
            source_path.display(),
            summary.root_compatibles,
            plan.dtb_compatibles
        ));
    }

    let staged_path = out_dir.join(plan.hardware.to_string()).join(dtb_esp_path);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&staged_path, &bytes)
        .map_err(|err| format!("write {}: {err}", staged_path.display()))?;

    staged.push(StagedAsset {
        source_path,
        esp_path: dtb_esp_path.clone(),
        staged_path,
        byte_len: bytes.len() as u64,
        fingerprint: fnv1a64(&bytes),
        dtb_model: summary.model,
    });

    Ok(staged)
}

fn image_manifest(
    plan: &ImagePlan,
    bootloader: Option<&StagedBootloader>,
    staged: &[StagedAsset],
) -> String {
    let mut manifest = plan.manifest();
    if let Some(asset) = bootloader {
        manifest.push_str(&format!(
            "bootloader_source_path={}\n",
            asset.source_path.display()
        ));
        manifest.push_str(&format!(
            "bootloader_esp_path={}\n",
            asset.esp_path.display()
        ));
        manifest.push_str(&format!(
            "bootloader_staged_path={}\n",
            asset.staged_path.display()
        ));
        manifest.push_str(&format!("bootloader_size={}\n", asset.byte_len));
        manifest.push_str(&format!(
            "bootloader_fingerprint=fnv1a64:{:016x}\n",
            asset.fingerprint
        ));
    }
    for asset in staged {
        manifest.push_str(&format!(
            "dtb_asset_source_path={}\n",
            asset.source_path.display()
        ));
        manifest.push_str(&format!(
            "dtb_asset_esp_path={}\n",
            asset.esp_path.display()
        ));
        manifest.push_str(&format!(
            "dtb_staged_path={}\n",
            asset.staged_path.display()
        ));
        manifest.push_str(&format!("dtb_size={}\n", asset.byte_len));
        manifest.push_str(&format!(
            "dtb_fingerprint=fnv1a64:{:016x}\n",
            asset.fingerprint
        ));
        if let Some(model) = &asset.dtb_model {
            manifest.push_str(&format!("dtb_model={model}\n"));
        }
    }
    manifest
}

#[derive(Debug)]
struct StagedKernel {
    source_path: PathBuf,
    esp_path: PathBuf,
    staged_path: PathBuf,
    byte_len: u64,
    fingerprint: u64,
    entry: u64,
}

#[derive(Debug)]
struct StagedSimpleAsset {
    esp_path: PathBuf,
    staged_path: PathBuf,
    byte_len: u64,
    fingerprint: u64,
}

fn stage_kernel(
    root: &Path,
    out_dir: &Path,
    plan: &ImagePlan,
) -> Result<Option<StagedKernel>, String> {
    let (target, features, bin_name) = match plan.arch {
        ImageArch::Aarch64 => ("aarch64-unknown-none", "arch_aarch64", "kumo-kernel"),
        ImageArch::X86_64 => ("x86_64-unknown-none", "arch_x86_64", "kumo-kernel"),
    };

    run_cargo(
        root,
        &[
            "build",
            "-p",
            "kernel",
            "--bin",
            bin_name,
            "--target",
            target,
            "--release",
            "--no-default-features",
            "--features",
            features,
        ],
    )?;

    let source_path = root
        .join("target")
        .join(target)
        .join("release")
        .join(bin_name);
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    let entry = match plan.arch {
        ImageArch::Aarch64 => validate_aarch64_kernel_elf(&bytes)
            .map_err(|err| format!("validate {} as kernel ELF: {err}", source_path.display()))?,
        ImageArch::X86_64 => validate_x86_64_kernel_elf(&bytes)
            .map_err(|err| format!("validate {} as kernel ELF: {err}", source_path.display()))?,
    };
    // Higher-half check only applies to aarch64 TTBR1.
    if plan.arch == ImageArch::Aarch64 && entry < 0xffff_0000_0000_0000 {
        return Err(format!(
            "validate {} as higher-half kernel ELF: entry {entry:#x} is not in TTBR1",
            source_path.display()
        ));
    }

    let staged_path = out_dir
        .join(plan.hardware.to_string())
        .join(KERNEL_ESP_PATH);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&staged_path, &bytes)
        .map_err(|err| format!("write {}: {err}", staged_path.display()))?;

    Ok(Some(StagedKernel {
        source_path,
        esp_path: PathBuf::from(KERNEL_ESP_PATH),
        staged_path,
        byte_len: bytes.len() as u64,
        fingerprint: fnv1a64(&bytes),
        entry,
    }))
}

/// Build a minimal FAT32 disk image with:
///   sector 0:   BPB
///   sector 1:   FSInfo (signatures only)
///   sectors 2–31: reserved (zeroed)
///   sectors 32–63: FAT #1
///   sectors 64–95: FAT #2
///   sector 96:  root directory (cluster 2) with a volume label + two test files
///
/// Layout constants (must be internally consistent):
const FAT_SEC_SIZE: u16 = 512;
const FAT_RESERVED: u16 = 32;
const FAT_NUM_FATS: u8 = 2;
const FAT_SPC: u8 = 1;
const FAT_SPF: u32 = 32; // sectors per FAT
const FAT_TOTAL_SECS: u32 = 4096;
const ROOT_CLUSTER: u32 = 2;
const DATA_START: u64 = FAT_RESERVED as u64 + (FAT_NUM_FATS as u64 * FAT_SPF as u64);

fn build_fat32_image() -> Vec<u8> {
    let total = FAT_TOTAL_SECS as usize * FAT_SEC_SIZE as usize;
    let mut img = vec![0u8; total];

    // ---- sector 0: BPB ----
    let s = &mut img[..512];
    s[0..3].copy_from_slice(&[0xEB, 0xFE, 0x90]);
    s[3..11].copy_from_slice(b"MSDOS5.0");
    s[0x0B..0x0D].copy_from_slice(&FAT_SEC_SIZE.to_le_bytes());
    s[0x0D] = FAT_SPC;
    s[0x0E..0x10].copy_from_slice(&FAT_RESERVED.to_le_bytes());
    s[0x10] = FAT_NUM_FATS;
    s[0x15] = 0xF8;
    s[0x18..0x1A].copy_from_slice(&63u16.to_le_bytes());
    s[0x1A..0x1C].copy_from_slice(&255u16.to_le_bytes());
    s[0x20..0x24].copy_from_slice(&FAT_TOTAL_SECS.to_le_bytes());
    s[0x24..0x28].copy_from_slice(&FAT_SPF.to_le_bytes());
    s[0x2C..0x30].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    s[0x30..0x32].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector
    s[0x32..0x34].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
    s[0x40] = 0x80;
    s[0x42] = 0x29;
    s[0x43..0x47].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
    s[0x47..0x52].copy_from_slice(b"KUMO       ");
    s[0x52..0x5A].copy_from_slice(b"FAT32   ");
    s[0x1FE..0x200].copy_from_slice(&[0x55, 0xAA]);

    // ---- sector 1: FSInfo (signatures only) ----
    img[512..516].copy_from_slice(b"RRaA");
    img[512 + 0x1E4..512 + 0x1E8].copy_from_slice(b"rrAa");

    // ---- FAT #1 at sector 32 ----
    let fat1_off = FAT_RESERVED as usize * FAT_SEC_SIZE as usize;
    let fat_size = FAT_SPF as usize * FAT_SEC_SIZE as usize;
    {
        let fat1 = &mut img[fat1_off..fat1_off + fat_size];
        fat1[0..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0x0F]);
        fat1[4..8].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x0F]);
        fat1[8..12].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x0F]); // cluster 2 = root dir = EOF
        fat1[12..16].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x0F]); // cluster 3 = HELLO.TXT = EOF
    }

    // ---- FAT #2 at sector 64 (copy of FAT #1) ----
    let fat2_off = fat1_off + fat_size;
    img.copy_within(fat1_off..fat1_off + fat_size, fat2_off);

    // ---- cluster 3 (sector 97): HELLO.TXT payload ----
    let cluster3_off =
        (DATA_START as usize + (3 - 2) as usize * FAT_SPC as usize) * FAT_SEC_SIZE as usize;
    img[cluster3_off..cluster3_off + 6].copy_from_slice(b"hello!");

    // ---- root directory at cluster 2 (sector 96) ----
    let root_off = DATA_START as usize * FAT_SEC_SIZE as usize;
    {
        let dir = &mut img[root_off..root_off + 512];

        fn put_entry(
            dir: &mut [u8],
            off: usize,
            name: &[u8; 11],
            attr: u8,
            cluster: u32,
            size: u32,
        ) {
            let e = &mut dir[off..off + 32];
            e[..11].copy_from_slice(name);
            e[11] = attr;
            e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
            e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
            e[28..32].copy_from_slice(&size.to_le_bytes());
        }

        put_entry(dir, 0, b"KUMO       ", 0x08, 0, 0);
        put_entry(dir, 32, b"README  TXT", 0x20, 0, 128);
        put_entry(dir, 64, b"HELLO   TXT", 0x20, 3, 6); // cluster 3, 6 bytes
        dir[96] = 0x00;
    }

    img
}

fn stage_initrd(out_dir: &Path, plan: &ImagePlan) -> Result<Option<StagedSimpleAsset>, String> {
    let initrd = match plan.arch {
        ImageArch::Aarch64 => {
            let sora = build_sora_image(&workspace_root()?)?;
            let svc_health = build_svc_health_image(&workspace_root()?)?;
            let drv_serial = build_drv_serial_image(&workspace_root()?)?;
            let fat32_img = build_fat32_image();
            let persona_linux_hello = build_persona_linux_hello_elf();
            build_initrd(&[
                (SORA_INIT_PATH, sora.as_slice()),
                (SVC_HEALTH_PATH, svc_health.as_slice()),
                (DRV_SERIAL_PATH, drv_serial.as_slice()),
                (FAT32_IMG_PATH, fat32_img.as_slice()),
                (PERSONA_LINUX_HELLO_PATH, persona_linux_hello.as_slice()),
            ])?
        }
        ImageArch::X86_64 => {
            // P10: empty initrd placeholder — Sora is aarch64-only. The kernel
            // boots without userspace; Nijigumo still needs a valid initrd image
            // to pass via BootInfo.
            build_initrd(&[])?
        }
    };

    let staged_path = out_dir
        .join(plan.hardware.to_string())
        .join(INITRD_ESP_PATH);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&staged_path, &initrd)
        .map_err(|err| format!("write {}: {err}", staged_path.display()))?;

    Ok(Some(StagedSimpleAsset {
        esp_path: PathBuf::from(INITRD_ESP_PATH),
        staged_path,
        byte_len: initrd.len() as u64,
        fingerprint: fnv1a64(&initrd),
    }))
}

fn build_sora_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "sora",
            "--bin",
            "sora",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("sora");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as Sora ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_svc_health_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "svc-health",
            "--bin",
            "svc-health",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("svc-health");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes).map_err(|err| {
        format!(
            "validate {} as svc-health ELF: {err}",
            source_path.display()
        )
    })?;
    Ok(bytes)
}

fn build_drv_serial_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "drv-serial",
            "--bin",
            "drv-serial",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("drv-serial");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes).map_err(|err| {
        format!(
            "validate {} as drv-serial ELF: {err}",
            source_path.display()
        )
    })?;
    Ok(bytes)
}

fn build_persona_linux_hello_elf() -> Vec<u8> {
    const ELF_HEADER_LEN: usize = 64;
    const ELF_PHDR_LEN: usize = 56;
    const ET_EXEC: u16 = 2;
    const EM_AARCH64: u16 = 0xb7;
    const PT_LOAD: u32 = 1;
    const PF_X: u32 = 1 << 0;
    const PF_R: u32 = 1 << 2;
    const ENTRY: u64 = 0x1000_1000;
    const DATA: u64 = 0x1000_2000;
    const CODE_OFFSET: usize = 0x1000;
    const DATA_OFFSET: usize = 0x2000;

    fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
        buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    let mut elf = vec![0u8; 0x3000];
    elf[0..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2; // ELFCLASS64
    elf[5] = 1; // little-endian
    elf[6] = 1; // current version
    put_u16(&mut elf, 16, ET_EXEC);
    put_u16(&mut elf, 18, EM_AARCH64);
    put_u32(&mut elf, 20, 1);
    put_u64(&mut elf, 24, ENTRY);
    put_u64(&mut elf, 32, ELF_HEADER_LEN as u64);
    put_u16(&mut elf, 52, ELF_HEADER_LEN as u16);
    put_u16(&mut elf, 54, ELF_PHDR_LEN as u16);
    put_u16(&mut elf, 56, 2);

    let code_ph = ELF_HEADER_LEN;
    put_u32(&mut elf, code_ph, PT_LOAD);
    put_u32(&mut elf, code_ph + 4, PF_R | PF_X);
    put_u64(&mut elf, code_ph + 8, CODE_OFFSET as u64);
    put_u64(&mut elf, code_ph + 16, ENTRY);
    put_u64(&mut elf, code_ph + 24, ENTRY);
    put_u64(&mut elf, code_ph + 32, 36);
    put_u64(&mut elf, code_ph + 40, 36);
    put_u64(&mut elf, code_ph + 48, 0x1000);

    let data_ph = ELF_HEADER_LEN + ELF_PHDR_LEN;
    let msg = b"M10 elf linux hi\n";
    put_u32(&mut elf, data_ph, PT_LOAD);
    put_u32(&mut elf, data_ph + 4, PF_R);
    put_u64(&mut elf, data_ph + 8, DATA_OFFSET as u64);
    put_u64(&mut elf, data_ph + 16, DATA);
    put_u64(&mut elf, data_ph + 24, DATA);
    put_u64(&mut elf, data_ph + 32, msg.len() as u64);
    put_u64(&mut elf, data_ph + 40, msg.len() as u64);
    put_u64(&mut elf, data_ph + 48, 0x1000);

    // Payload verified with `llvm-mc -triple=aarch64 --show-encoding`.
    let code: [u32; 9] = [
        0xd2800020, // movz x0, #1
        0xd2a20001, // movz x1, #0x1000, lsl #16
        0xf2840001, // movk x1, #0x2000
        0xd2800222, // movz x2, #17
        0xd2800808, // movz x8, #64
        0xd4000001, // svc #0
        0xd2800000, // movz x0, #0
        0xd2800bc8, // movz x8, #94
        0xd4000001, // svc #0
    ];
    for (index, word) in code.iter().enumerate() {
        elf[CODE_OFFSET + index * 4..CODE_OFFSET + (index + 1) * 4]
            .copy_from_slice(&word.to_le_bytes());
    }
    elf[DATA_OFFSET..DATA_OFFSET + msg.len()].copy_from_slice(msg);
    elf
}

fn build_initrd(files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let table_bytes = files
        .len()
        .checked_mul(INITRD_ENTRY_LEN)
        .ok_or_else(|| "initrd entry table too large".to_owned())?;
    let data_offset = INITRD_HEADER_LEN
        .checked_add(table_bytes)
        .ok_or_else(|| "initrd header too large".to_owned())?;
    let mut initrd = vec![0; data_offset];

    initrd[..8].copy_from_slice(&INITRD_MAGIC);
    initrd[8..12].copy_from_slice(&INITRD_VERSION.to_le_bytes());
    initrd[12..16].copy_from_slice(&(files.len() as u32).to_le_bytes());

    let mut cursor = data_offset;
    for (index, (path, bytes)) in files.iter().enumerate() {
        let path_bytes = path.as_bytes();
        if path_bytes.is_empty() || path_bytes.len() >= INITRD_PATH_MAX {
            return Err(format!("initrd path '{path}' does not fit"));
        }

        let entry = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN;
        initrd[entry..entry + path_bytes.len()].copy_from_slice(path_bytes);
        initrd[entry + INITRD_PATH_MAX..entry + INITRD_PATH_MAX + 8]
            .copy_from_slice(&(cursor as u64).to_le_bytes());
        initrd[entry + INITRD_PATH_MAX + 8..entry + INITRD_PATH_MAX + 16]
            .copy_from_slice(&(bytes.len() as u64).to_le_bytes());

        initrd.extend_from_slice(bytes);
        cursor = initrd.len();
    }

    Ok(initrd)
}

fn validate_x86_64_kernel_elf(bytes: &[u8]) -> Result<u64, String> {
    const ET_EXEC: u16 = 2;
    const EM_X86_64: u16 = 0x3E;

    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return Err("missing ELF magic".to_owned());
    }
    if bytes[4] != 2 {
        return Err("not ELFCLASS64".to_owned());
    }
    if bytes[5] != 1 {
        return Err("not little-endian".to_owned());
    }
    let e_type = read_le_u16(bytes, 16)?;
    if e_type != ET_EXEC {
        return Err(format!("e_type is {e_type}, expected EXEC (2)"));
    }
    let e_machine = read_le_u16(bytes, 18)?;
    if e_machine != EM_X86_64 {
        return Err(format!("e_machine is 0x{e_machine:04x}, expected x86_64"));
    }
    read_le_u64(bytes, 24)
}

fn validate_aarch64_kernel_elf(bytes: &[u8]) -> Result<u64, String> {
    const ET_EXEC: u16 = 2;
    const EM_AARCH64: u16 = 0xB7;

    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return Err("missing ELF magic".to_owned());
    }
    if bytes[4] != 2 {
        return Err("not ELFCLASS64".to_owned());
    }
    if bytes[5] != 1 {
        return Err("not little-endian".to_owned());
    }
    let e_type = read_le_u16(bytes, 16)?;
    if e_type != ET_EXEC {
        return Err(format!("e_type is {e_type}, expected EXEC (2)"));
    }
    let e_machine = read_le_u16(bytes, 18)?;
    if e_machine != EM_AARCH64 {
        return Err(format!("e_machine is 0x{e_machine:04x}, expected AArch64"));
    }
    read_le_u64(bytes, 24)
}

fn validate_aarch64_efi_application(bytes: &[u8]) -> Result<(), String> {
    const ARM64_MACHINE: u16 = 0xaa64;
    const PE32_PLUS: u16 = 0x20b;
    const EFI_APPLICATION_SUBSYSTEM: u16 = 10;

    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return Err("missing DOS MZ header".to_owned());
    }

    let pe_offset = read_le_u32(bytes, 0x3c)? as usize;
    let signature_end = pe_offset
        .checked_add(4)
        .ok_or_else(|| "invalid PE signature offset".to_owned())?;
    if signature_end > bytes.len() || &bytes[pe_offset..signature_end] != b"PE\0\0" {
        return Err("missing PE signature".to_owned());
    }

    let machine = read_le_u16(bytes, pe_offset + 4)?;
    if machine != ARM64_MACHINE {
        return Err(format!("PE machine is 0x{machine:04x}, expected 0xaa64"));
    }

    let optional_header_size = read_le_u16(bytes, pe_offset + 20)? as usize;
    let optional_header = pe_offset
        .checked_add(24)
        .ok_or_else(|| "invalid PE optional-header offset".to_owned())?;
    if optional_header_size < 70 {
        return Err(format!(
            "PE optional header too small ({optional_header_size} bytes)"
        ));
    }

    let magic = read_le_u16(bytes, optional_header)?;
    if magic != PE32_PLUS {
        return Err(format!(
            "PE optional-header magic is 0x{magic:04x}, expected PE32+"
        ));
    }

    let subsystem = read_le_u16(bytes, optional_header + 68)?;
    if subsystem != EFI_APPLICATION_SUBSYSTEM {
        return Err(format!(
            "PE subsystem is {subsystem}, expected EFI application"
        ));
    }

    Ok(())
}

fn validate_x86_64_efi_application(bytes: &[u8]) -> Result<(), String> {
    const AMD64_MACHINE: u16 = 0x8664;
    const PE32_PLUS: u16 = 0x20b;
    const EFI_APPLICATION_SUBSYSTEM: u16 = 10;

    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return Err("missing DOS MZ header".to_owned());
    }
    let pe_offset = read_le_u32(bytes, 0x3c)? as usize;
    let sig_end = pe_offset.checked_add(4).ok_or("invalid PE offset")?;
    if sig_end > bytes.len() || &bytes[pe_offset..sig_end] != b"PE\0\0" {
        return Err("missing PE signature".to_owned());
    }
    let machine = read_le_u16(bytes, pe_offset + 4)?;
    if machine != AMD64_MACHINE {
        return Err(format!("PE machine 0x{machine:04x}, expected 0x8664"));
    }
    let opt_hdr_size = read_le_u16(bytes, pe_offset + 20)? as usize;
    let opt_hdr = pe_offset.checked_add(24).ok_or("invalid PE optional-hdr")?;
    if opt_hdr_size < 70 {
        return Err(format!("PE optional header too small ({opt_hdr_size})"));
    }
    let magic = read_le_u16(bytes, opt_hdr)?;
    if magic != PE32_PLUS {
        return Err(format!(
            "PE optional-header magic 0x{magic:04x}, expected PE32+"
        ));
    }
    let subsystem = read_le_u16(bytes, opt_hdr + 68)?;
    if subsystem != EFI_APPLICATION_SUBSYSTEM {
        return Err(format!(
            "PE subsystem {subsystem}, expected EFI application"
        ));
    }
    Ok(())
}

fn read_le_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| "integer overflow while reading u16".to_owned())?;
    if end > bytes.len() {
        return Err(format!("offset {offset} is outside {} bytes", bytes.len()));
    }
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "integer overflow while reading u32".to_owned())?;
    if end > bytes.len() {
        return Err(format!("offset {offset} is outside {} bytes", bytes.len()));
    }
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn read_le_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| "integer overflow while reading u64".to_owned())?;
    if end > bytes.len() {
        return Err(format!("offset {offset} is outside {} bytes", bytes.len()));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..end]);
    Ok(u64::from_le_bytes(buf))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn run_smoke(root: &Path, arch: Arch) -> Result<(), String> {
    build(root, arch)?;
    image(
        root,
        arch,
        HardwareTarget::default_for_arch(arch.image_arch()),
    )?;
    let products = build_products(root)?;
    run_product_self_test(&products.host_stage)?;
    maybe_run_qemu(&products.arm64_qemu)?;
    println!("KUMO Stage-A host smoke complete for {arch}");
    println!("Testable host product: {}", products.host_stage.display());
    println!(
        "Arm64 QEMU boot image: {}",
        products.arm64_qemu.image.display()
    );
    println!("UEFI/AAVMF boot is still deferred until Nijigumo has a real UEFI entry.");
    Ok(())
}

/// Run the mechanical guardrail tripwires (GUIDANCE/006 §5): fmt, the VEIL-identifier
/// grep, and the kernel register-leak ratchet. Delegates to `scripts/preflight.sh` so the
/// same checks run identically from the shell and from CI. Pass `--full` through env
/// (`KUMO_PREFLIGHT_FULL=1 cargo xtask preflight`) to also build both backends + smoke.
fn preflight(root: &Path) -> Result<(), String> {
    let script = root.join("scripts/preflight.sh");
    let mut cmd = Command::new("sh");
    cmd.arg(&script).current_dir(root);
    if env::var_os("KUMO_PREFLIGHT_FULL").is_some() {
        cmd.arg("--full");
    }
    let status = cmd
        .status()
        .map_err(|err| format!("spawn {}: {err}", script.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("preflight tripwire failed with {status}"))
    }
}

fn run_cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|err| format!("spawn cargo {}: {err}", args.join(" ")))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed with {status}", args.join(" ")))
    }
}

fn check_kernel_backend(root: &Path, arch: Arch) -> Result<(), String> {
    run_cargo(
        root,
        &[
            "check",
            "-p",
            "kernel",
            "--no-default-features",
            "--features",
            arch.kernel_feature(),
        ],
    )
}

fn test_kernel_backend(root: &Path, arch: Arch) -> Result<(), String> {
    run_cargo(
        root,
        &[
            "test",
            "-p",
            "kernel",
            "--no-default-features",
            "--features",
            arch.kernel_feature(),
        ],
    )
}

#[derive(Debug)]
struct Products {
    host_stage: PathBuf,
    arm64_qemu: Arm64QemuBootFiles,
}

#[derive(Debug)]
struct Arm64QemuBootFiles {
    elf: PathBuf,
    image: PathBuf,
    object: PathBuf,
    readme: PathBuf,
    run_script: PathBuf,
    expected_serial: PathBuf,
}

fn build_products(root: &Path) -> Result<Products, String> {
    Ok(Products {
        host_stage: build_host_stage_product(root)?,
        arm64_qemu: build_arm64_qemu_boot_files(root)?,
    })
}

fn build_host_stage_product(root: &Path) -> Result<PathBuf, String> {
    run_cargo(root, &["build", "-p", "kumo-stage-a-smoke"])?;

    let src = root
        .join("target/debug")
        .join(format!("kumo-stage-a-smoke{}", env::consts::EXE_SUFFIX));
    let dst = host_stage_product_path(root);
    let out_dir = dst
        .parent()
        .ok_or_else(|| format!("invalid product path {}", dst.display()))?;
    fs::create_dir_all(out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;
    fs::copy(&src, &dst)
        .map_err(|err| format!("copy {} to {}: {err}", src.display(), dst.display()))?;

    Ok(dst)
}

fn build_arm64_qemu_boot_files(root: &Path) -> Result<Arm64QemuBootFiles, String> {
    let src_dir = root.join("boot/niji-raw-aarch64/qemu-virt");
    let out_dir = root.join("build/aarch64/qemu-virt");
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let files = Arm64QemuBootFiles {
        object: out_dir.join("stage_a.o"),
        elf: out_dir.join("kumo-qemu-virt.elf"),
        image: out_dir.join("kumo-qemu-virt.img"),
        readme: out_dir.join("README.txt"),
        run_script: out_dir.join("run-qemu.sh"),
        expected_serial: out_dir.join("expected-serial.txt"),
    };

    run_tool(
        root,
        "clang",
        &[
            "-target",
            "aarch64-none-elf",
            "-c",
            path_arg(&src_dir.join("stage_a.S"))?,
            "-o",
            path_arg(&files.object)?,
        ],
    )?;
    run_tool(
        root,
        "ld.lld",
        &[
            "-T",
            path_arg(&src_dir.join("link.ld"))?,
            "-nostdlib",
            "-o",
            path_arg(&files.elf)?,
            path_arg(&files.object)?,
        ],
    )?;
    run_tool(
        root,
        "llvm-objcopy",
        &[
            "-O",
            "binary",
            path_arg(&files.elf)?,
            path_arg(&files.image)?,
        ],
    )?;

    fs::write(&files.readme, boot_readme(&files))
        .map_err(|err| format!("write {}: {err}", files.readme.display()))?;
    fs::write(&files.expected_serial, expected_arm64_serial())
        .map_err(|err| format!("write {}: {err}", files.expected_serial.display()))?;
    fs::write(&files.run_script, qemu_run_script())
        .map_err(|err| format!("write {}: {err}", files.run_script.display()))?;

    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&files.run_script)
            .map_err(|err| format!("metadata {}: {err}", files.run_script.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&files.run_script, perms)
            .map_err(|err| format!("chmod {}: {err}", files.run_script.display()))?;
    }

    Ok(files)
}

fn verify_arm64_qemu_boot_files(files: &Arm64QemuBootFiles) -> Result<(), String> {
    let image_len = fs::metadata(&files.image)
        .map_err(|err| format!("metadata {}: {err}", files.image.display()))?
        .len();
    if image_len == 0 {
        return Err(format!("{} is empty", files.image.display()));
    }

    run_tool(
        files
            .elf
            .parent()
            .ok_or_else(|| format!("invalid ELF path {}", files.elf.display()))?,
        "llvm-readelf",
        &["-h", path_arg(&files.elf)?],
    )?;
    println!(
        "KUMO arm64 boot files verified: {} ({} bytes)",
        files.image.display(),
        image_len
    );
    Ok(())
}

fn maybe_run_qemu(files: &Arm64QemuBootFiles) -> Result<(), String> {
    if command_exists("qemu-system-aarch64") {
        println!(
            "qemu-system-aarch64 found; interactive boot script is {}",
            files.run_script.display()
        );
    } else {
        println!(
            "qemu-system-aarch64 not found; boot with {} once QEMU is installed",
            files.run_script.display()
        );
    }
    Ok(())
}

fn run_qemu_smoke_if_available(files: &Arm64QemuBootFiles) -> Result<(), String> {
    if !command_exists("qemu-system-aarch64") {
        println!("qemu-system-aarch64 not found; skipping arm64 QEMU smoke");
        return Ok(());
    }

    run_qemu_serial_smoke(files)
}

fn run_qemu_serial_smoke(files: &Arm64QemuBootFiles) -> Result<(), String> {
    let mut child = Command::new("qemu-system-aarch64")
        .args([
            "-M",
            "virt",
            "-cpu",
            "cortex-a72",
            "-display",
            "none",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-kernel",
            path_arg(&files.elf)?,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn qemu-system-aarch64 qemu smoke: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "qemu smoke stdout unavailable".to_owned())?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = stdout;
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buffer[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "qemu smoke stdin unavailable".to_owned())?;

    let smoke_result: Result<(), String> = (|| {
        let mut transcript = Vec::new();
        read_serial_until(
            &rx,
            &mut transcript,
            "boot transcript",
            &[b"[NIJIGUMO] HANDOFF COMPLETE", b"READY\n"],
            Duration::from_secs(3),
        )?;

        stdin
            .write_all(b"HELLO\r")
            .map_err(|err| format!("write qemu smoke serial input: {err}"))?;
        stdin
            .flush()
            .map_err(|err| format!("flush qemu smoke serial input: {err}"))?;
        read_serial_until(
            &rx,
            &mut transcript,
            "serial echo",
            &[b"HELLO\r\n"],
            Duration::from_secs(3),
        )?;

        stdin
            .write_all(b"AB\x7fC\r")
            .map_err(|err| format!("write qemu smoke delete input: {err}"))?;
        stdin
            .flush()
            .map_err(|err| format!("flush qemu smoke delete input: {err}"))?;
        read_serial_until(
            &rx,
            &mut transcript,
            "serial delete echo",
            &[b"AB\x08 \x08C\r\n"],
            Duration::from_secs(3),
        )?;
        Ok(())
    })();

    stop_qemu_child(&mut child);

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }

    smoke_result?;

    println!("KUMO QEMU smoke green: READY reached, serial echo worked, Delete erased");
    Ok(())
}

fn read_serial_until(
    rx: &Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    context: &str,
    needles: &[&[u8]],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if needles
            .iter()
            .all(|needle| contains_bytes(transcript, needle))
        {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(format!(
                "qemu serial {context} missing {:?}; got {:?}",
                needles
                    .iter()
                    .map(|needle| String::from_utf8_lossy(needle).into_owned())
                    .collect::<Vec<_>>(),
                String::from_utf8_lossy(transcript)
            ));
        }

        let remaining = deadline.saturating_duration_since(now);
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(bytes) => transcript.extend_from_slice(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "qemu serial stream closed while waiting for {context}"
                ));
            }
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn stop_qemu_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn host_stage_product_path(root: &Path) -> PathBuf {
    root.join("build/host")
        .join(format!("kumo-stage-a-smoke{}", env::consts::EXE_SUFFIX))
}

fn run_product_self_test(path: &Path) -> Result<(), String> {
    let status = Command::new(path)
        .arg("--self-test")
        .status()
        .map_err(|err| format!("run {} --self-test: {err}", path.display()))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} --self-test failed with {status}",
            path.display()
        ))
    }
}

fn run_tool(cwd: &Path, program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|err| format!("spawn {} {}: {err}", program, args.join(" ")))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} {} failed with {status}",
            program,
            args.join(" ")
        ))
    }
}

fn path_arg(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not UTF-8: {}", path.display()))
}

fn command_exists(program: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

fn expected_arm64_serial() -> &'static str {
    "[NIJIGUMO] HANDOFF COMPLETE abi=v1 arch=aarch64\n\
CPU MODE: Executive (EL1)\n\
AETHER: pending; boot map=QEMU-virt handoff unavailable in raw path\n\
READY\n"
}

fn qemu_run_script() -> &'static str {
    "#!/bin/sh\n\
set -eu\n\
DIR=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
exec qemu-system-aarch64 \\\n\
    -M virt \\\n\
    -cpu cortex-a72 \\\n\
    -display none \\\n\
    -serial stdio \\\n\
    -monitor none \\\n\
    -kernel \"$DIR/kumo-qemu-virt.elf\"\n"
}

fn boot_readme(files: &Arm64QemuBootFiles) -> String {
    format!(
        "KUMO arm64 QEMU virt boot files\n\n\
ELF: {}\n\
Raw image: {}\n\
Run script: {}\n\n\
Run once qemu-system-aarch64 is installed:\n\
  {}\n\n\
Expected serial transcript is in:\n\
  {}\n",
        files.elf.display(),
        files.image.display(),
        files.run_script.display(),
        files.run_script.display(),
        files.expected_serial.display()
    )
}

fn print_help() {
    println!(
        "usage: cargo xtask <build|test|boot-files|qemu-smoke|image|product|run|preflight> [--arch aarch64|x86_64] [--hardware x13s|qemu-virt-aarch64|generic-uefi-x86_64]"
    );
    println!("default arch: aarch64; default hardware: thinkpad-x13s-gen1");
    println!("preflight: mechanical guardrail tripwires (GUIDANCE/006 §5); KUMO_PREFLIGHT_FULL=1 adds both-backend build + smoke");
}
