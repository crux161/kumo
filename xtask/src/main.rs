//j456
//j457
//j460
//j467
//j493

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
    ARGS_PATH, AUTOEXEC_PATH, CAT_PATH, DRV_BLK_PATH, DRV_FB_PATH, DRV_I2C_HID_PATH,
    DRV_SERIAL_PATH, DRV_XHCI_PATH, HELLO_PATH, INITRD_ENTRY_LEN, INITRD_HEADER_LEN, INITRD_MAGIC,
    INITRD_PATH_MAX, INITRD_VERSION, LS_PATH, LUA_REPL_PATH, PERSONA_LINUX_HELLO_PATH,
    SORA_INIT_PATH, SVC_HEALTH_PATH, THREADS_PATH, TTYD_PATH, WC_PATH,
};

const FAT32_IMG_PATH: &str = "bin/fat32.img";

/// Staged kernel image + initrd locations on the ESP (must match the paths
/// `niji-uefi` opens at runtime).
const KERNEL_ESP_PATH: &str = "EFI/KUMO/kernel/kumo-kernel.elf";
const INITRD_ESP_PATH: &str = "EFI/KUMO/initrd.img";
/// The boot manifest Nijigumo reads from the ESP. Must match `BOOT_CONFIG_ESP_PATH` in
/// `boot/niji-uefi/src/main.rs` (the same path in UEFI's backslash form).
const BOOT_MANIFEST_ESP_PATH: &str = "EFI/KUMO/nijigumo.conf";
const X86_MULTIBOOT_INITRD_PATH: &str = "target/x86_64-unknown-none/release/kumo-initrd.img";

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
    pl011_console_base: Option<u64>,
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
    if args.pl011_console_base.is_some() && args.command != "image" {
        return Err("--console-uart is valid only for `cargo xtask image`".to_owned());
    }
    if args.pl011_console_base.is_some() && args.arch != Arch::Aarch64 {
        return Err("--console-uart currently supports only aarch64 PL011 routes".to_owned());
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
        "x86-smoke" => run_x86_qemu_smoke(&root),
        "x86-uefi-smoke" => run_x86_uefi_smoke(&root),
        "x86-initrd" => {
            let initrd = build_x86_multiboot_initrd(&root)?;
            println!("{}", initrd.display());
            Ok(())
        }
        "image" => image(&root, args.arch, hardware, args.pl011_console_base),
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
    let mut pl011_console_base = None;

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
            "--console-uart" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--console-uart requires a value".to_owned())?;
                pl011_console_base = Some(parse_pl011_console_arg(&value)?);
            }
            "-h" | "--help" => {
                return Ok(Args {
                    command: "help".to_owned(),
                    arch,
                    hardware,
                    pl011_console_base,
                });
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }

    Ok(Args {
        command,
        arch,
        hardware,
        pl011_console_base,
    })
}

fn parse_pl011_console_arg(value: &str) -> Result<u64, String> {
    let (kind, address) = value
        .trim()
        .split_once('@')
        .ok_or_else(|| "--console-uart expects pl011@0x<physical-base>".to_owned())?;
    if !kind.eq_ignore_ascii_case("pl011") {
        return Err("--console-uart currently supports only the pl011 kind".to_owned());
    }
    let digits = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))
        .ok_or_else(|| "--console-uart address must start with 0x".to_owned())?;
    let base = u64::from_str_radix(digits, 16)
        .map_err(|_| "--console-uart address is not valid hexadecimal".to_owned())?;
    if base == 0 || base >= (1u64 << 48) || base & 0xfff != 0 {
        return Err(
            "--console-uart address must be nonzero, 4 KiB-aligned, and below 2^48".to_owned(),
        );
    }
    Ok(base)
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
        "opi5plus"
        | "orange-pi-5-plus"
        | "orangepi-5-plus"
        | "orangepi5plus"
        | "rk3588-orangepi-5-plus" => Ok(HardwareTarget::OrangePi5Plus),
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

fn image(
    root: &Path,
    arch: Arch,
    hardware: HardwareTarget,
    pl011_console_base: Option<u64>,
) -> Result<(), String> {
    let out_dir = root.join("build/images");
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;

    let plan = ImagePlan::new("", hardware);
    let bootloader = stage_uefi_bootloader(root, &out_dir, &plan)?;
    let staged = stage_image_assets(root, &out_dir, &plan)?;
    let kernel = stage_kernel(root, &out_dir, &plan)?;
    let initrd = stage_initrd(&out_dir, &plan)?;
    let boot_manifest_path = stage_boot_manifest(&out_dir, &plan, pl011_console_base)?;
    let mut manifest = image_manifest(&plan, bootloader.as_ref(), &staged);
    manifest.push_str(&format!(
        "boot_manifest_esp_path={BOOT_MANIFEST_ESP_PATH}\nboot_manifest_staged_path={}\n",
        boot_manifest_path.display()
    ));
    manifest.push_str(&format!(
        "board_id={}\n",
        plan.hardware.board().map(|b| b.id()).unwrap_or("")
    ));
    if let Some(base) = pl011_console_base {
        manifest.push_str(&format!("pl011_console_base=0x{base:016x}\n"));
    }
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

/// Rewrite an imager ESP path (`/`-separated, relative) into the absolute backslash form
/// UEFI's file protocol expects, which is how the manifest names its files.
fn esp_uefi_path(path: &str) -> String {
    format!("\\{}", path.replace('/', "\\"))
}

/// Render the declarative boot manifest Nijigumo reads from the ESP
/// (`niji_uefi::BootConfig`). An image is built for exactly one board, so the board identity
/// is a build-time fact stamped here rather than one the kernel sniffs from present hardware
/// (DESIGN/017 §3); the loader forwards it into `BootInfo` and the kernel resolves it with
/// `kumo_bsp::Board::from_id`.
///
/// The manifest already describes the kernel/initrd/DTB the loader should take. Nijigumo still
/// reads those paths from its own constants and consumes only the fixed-size `board` and optional
/// `console-uart` policy — migrating the path reads onto the manifest is a later slice, and the
/// parser ignores keys it does not use.
fn render_boot_manifest(plan: &ImagePlan, pl011_console_base: Option<u64>) -> String {
    let mut out =
        String::from("# KUMO boot manifest - generated by `cargo xtask image`. Do not edit.\n");
    out.push_str(&format!("kernel = {}\n", esp_uefi_path(KERNEL_ESP_PATH)));
    out.push_str(&format!("initrd = {}\n", esp_uefi_path(INITRD_ESP_PATH)));
    match &plan.dtb_path {
        Some(dtb) => out.push_str(&format!(
            "dtb = esp:{}\n",
            esp_uefi_path(&dtb.to_string_lossy())
        )),
        None => out.push_str("dtb = firmware\n"),
    }
    // x86_64 has no aarch64 BSP entry, so it stamps no board and the kernel reports it
    // unstamped — the same as before this manifest existed.
    if let Some(board) = plan.hardware.board() {
        out.push_str(&format!("board = {}\n", board.id()));
    }
    if let Some(base) = pl011_console_base {
        out.push_str(&format!("console-uart = pl011@0x{base:016x}\n"));
    }
    out
}

/// Write the boot manifest into the staged ESP tree. Returns its staged path.
fn stage_boot_manifest(
    out_dir: &Path,
    plan: &ImagePlan,
    pl011_console_base: Option<u64>,
) -> Result<PathBuf, String> {
    let staged_path = out_dir
        .join(plan.hardware.to_string())
        .join(BOOT_MANIFEST_ESP_PATH);
    if let Some(parent) = staged_path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&staged_path, render_boot_manifest(plan, pl011_console_base))
        .map_err(|err| format!("write {}: {err}", staged_path.display()))?;
    Ok(staged_path)
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
///   sector 96:  root directory (cluster 2): volume label + README/HELLO + EFI/
///   sector 97:  HELLO.TXT data (cluster 3)
///   sector 98:  EFI/ directory (cluster 4)
///   sector 99:  EFI/BOOT/ directory (cluster 5)
///   sector 100: EFI/BOOT/BOOTAA64.EFI data (cluster 6)
///
/// The `EFI/BOOT/BOOTAA64.EFI` subtree mirrors the ESP shape so the reader's path
/// resolution (`kumo-fatfs::resolve_path`) can be exercised against a real image;
/// the total size is unchanged (the clusters were already inside the 2 MiB image),
/// so the initrd layout — and `drv-blk`'s view of it — is untouched.
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

/// Byte offset of the first sector of `cluster` within the FAT image.
fn fat_cluster_off(cluster: u32) -> usize {
    (DATA_START as usize + (cluster as usize - 2) * FAT_SPC as usize) * FAT_SEC_SIZE as usize
}

/// Write a 32-byte short (8.3) directory entry at `off` within a directory slice.
fn put_dir_entry(dir: &mut [u8], off: usize, name: &[u8; 11], attr: u8, cluster: u32, size: u32) {
    let e = &mut dir[off..off + 32];
    e[..11].copy_from_slice(name);
    e[11] = attr;
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
}

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
        let eof: [u8; 4] = [0xFF, 0xFF, 0xFF, 0x0F];
        fat1[0..4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0x0F]); // media byte + reserved
        fat1[4..8].copy_from_slice(&eof); // cluster 1 reserved
        fat1[8..12].copy_from_slice(&eof); // cluster 2 = root dir = EOF
        fat1[12..16].copy_from_slice(&eof); // cluster 3 = HELLO.TXT = EOF
        fat1[16..20].copy_from_slice(&eof); // cluster 4 = EFI/ dir = EOF
        fat1[20..24].copy_from_slice(&eof); // cluster 5 = EFI/BOOT/ dir = EOF
        fat1[24..28].copy_from_slice(&eof); // cluster 6 = BOOTAA64.EFI = EOF
    }

    // ---- FAT #2 at sector 64 (copy of FAT #1) ----
    let fat2_off = fat1_off + fat_size;
    img.copy_within(fat1_off..fat1_off + fat_size, fat2_off);

    // ---- cluster 3 (sector 97): HELLO.TXT payload ----
    img[fat_cluster_off(3)..fat_cluster_off(3) + 6].copy_from_slice(b"hello!");

    // ---- root directory at cluster 2 (sector 96) ----
    {
        let dir = &mut img[fat_cluster_off(2)..fat_cluster_off(2) + 512];
        put_dir_entry(dir, 0, b"KUMO       ", 0x08, 0, 0); // volume label
        put_dir_entry(dir, 32, b"README  TXT", 0x20, 0, 128);
        put_dir_entry(dir, 64, b"HELLO   TXT", 0x20, 3, 6); // cluster 3, 6 bytes
        put_dir_entry(dir, 96, b"EFI        ", 0x10, 4, 0); // subdirectory, cluster 4
        dir[128] = 0x00; // end of directory
    }

    // ---- EFI/ directory at cluster 4 (sector 98) ----
    // A real subdirectory opens with `.` (itself) and `..` (parent; the root is
    // denoted by cluster 0); kumo-fatfs skips both while scanning for a name.
    {
        let dir = &mut img[fat_cluster_off(4)..fat_cluster_off(4) + 512];
        put_dir_entry(dir, 0, b".          ", 0x10, 4, 0);
        put_dir_entry(dir, 32, b"..         ", 0x10, 0, 0);
        put_dir_entry(dir, 64, b"BOOT       ", 0x10, 5, 0); // subdirectory, cluster 5
        dir[96] = 0x00;
    }

    // ---- EFI/BOOT/ directory at cluster 5 (sector 99) ----
    {
        let dir = &mut img[fat_cluster_off(5)..fat_cluster_off(5) + 512];
        put_dir_entry(dir, 0, b".          ", 0x10, 5, 0);
        put_dir_entry(dir, 32, b"..         ", 0x10, 4, 0);
        put_dir_entry(dir, 64, b"BOOTAA64EFI", 0x20, 6, 6); // cluster 6, 6 bytes
        dir[96] = 0x00;
    }

    // ---- cluster 6 (sector 100): EFI/BOOT/BOOTAA64.EFI payload ----
    img[fat_cluster_off(6)..fat_cluster_off(6) + 6].copy_from_slice(b"esp-ok");

    img
}

fn stage_initrd(out_dir: &Path, plan: &ImagePlan) -> Result<Option<StagedSimpleAsset>, String> {
    let initrd = match plan.arch {
        ImageArch::Aarch64 => {
            let sora = build_sora_image(&workspace_root()?)?;
            let svc_health = build_svc_health_image(&workspace_root()?)?;
            let ttyd = build_ttyd_image(&workspace_root()?)?;
            let drv_serial = build_drv_serial_image(&workspace_root()?)?;
            let drv_fb = build_drv_fb_image(&workspace_root()?)?;
            let drv_i2c_hid = build_drv_i2c_hid_image(&workspace_root()?)?;
            let drv_xhci = build_drv_xhci_image(&workspace_root()?)?;
            let drv_blk = build_drv_blk_image(&workspace_root()?)?;
            let fat32_img = build_fat32_image();
            let persona_linux_hello = build_persona_linux_hello_elf();
            let hello = build_hello_image(&workspace_root()?)?;
            let ls = build_ls_image(&workspace_root()?)?;
            let args = build_args_image(&workspace_root()?)?;
            let cat = build_cat_image(&workspace_root()?)?;
            let wc = build_wc_image(&workspace_root()?)?;
            let lua_repl = build_lua_repl_image(&workspace_root()?)?;
            let threads = build_threads_image(&workspace_root()?)?;
            let autoexec = build_autoexec();
            build_initrd(&[
                (SORA_INIT_PATH, sora.as_slice()),
                (SVC_HEALTH_PATH, svc_health.as_slice()),
                (TTYD_PATH, ttyd.as_slice()),
                (DRV_SERIAL_PATH, drv_serial.as_slice()),
                (DRV_FB_PATH, drv_fb.as_slice()),
                (DRV_I2C_HID_PATH, drv_i2c_hid.as_slice()),
                (DRV_XHCI_PATH, drv_xhci.as_slice()),
                (DRV_BLK_PATH, drv_blk.as_slice()),
                (FAT32_IMG_PATH, fat32_img.as_slice()),
                (PERSONA_LINUX_HELLO_PATH, persona_linux_hello.as_slice()),
                (HELLO_PATH, hello.as_slice()),
                (LS_PATH, ls.as_slice()),
                (ARGS_PATH, args.as_slice()),
                (CAT_PATH, cat.as_slice()),
                (WC_PATH, wc.as_slice()),
                (LUA_REPL_PATH, lua_repl.as_slice()),
                (THREADS_PATH, threads.as_slice()),
                (AUTOEXEC_PATH, autoexec.as_slice()),
            ])?
        }
        ImageArch::X86_64 => {
            let hello = build_x86_hello_image(&workspace_root()?)?;
            build_initrd(&[(HELLO_PATH, hello.as_slice())])?
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

fn build_ttyd_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "ttyd",
            "--bin",
            "ttyd",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("ttyd");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as ttyd ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

/// The boot autoexec manifest shipped at `etc/autoexec`: one shell command per line,
/// `#` comments and blanks ignored (`kumoza::autoexec_lines`), each dispatched by Sora's
/// shared `eval_command`. `echo` proves a builtin runs at boot, `ls` lists what's
/// installed, `run hello` launches a program, and `run args alpha beta` proves argument
/// passing (the program echoes its argv), and `cat` combines argv with a granted read-only
/// initrd capability — together exercising the shared evaluator end to end; the comment
/// line proves comment-skipping.
fn build_autoexec() -> Vec<u8> {
    b"# KUMO autoexec: shell command lines; '#' comments and blank lines ignored\n\
      echo kumo autoexec online\n\
      ls\n\
      run hello\n\
      run args alpha beta\n\
      cat etc/autoexec\n\
      wc etc/autoexec\n\
      threads\n"
        .to_vec()
}

fn build_hello_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "hello",
            "--bin",
            "hello",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("hello");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as hello ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_x86_hello_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "hello",
            "--bin",
            "hello",
            "--target",
            "x86_64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/x86_64-unknown-none/release")
        .join("hello");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_x86_64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as hello ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_x86_multiboot_initrd(root: &Path) -> Result<PathBuf, String> {
    let hello = build_x86_hello_image(root)?;
    let initrd = build_initrd(&[(HELLO_PATH, hello.as_slice())])?;
    let hello_file = kumo_abi::find_file(&initrd, HELLO_PATH)
        .map_err(|err| format!("validate x86 initrd: {err:?}"))?
        .ok_or_else(|| format!("x86 initrd is missing {HELLO_PATH}"))?;
    validate_x86_64_kernel_elf(hello_file.bytes)
        .map_err(|err| format!("validate x86 initrd {HELLO_PATH}: {err}"))?;

    let path = root.join(X86_MULTIBOOT_INITRD_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create {}: {err}", parent.display()))?;
    }
    fs::write(&path, initrd).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(path)
}

fn build_ls_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "ls",
            "--bin",
            "ls",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root.join("target/aarch64-unknown-none/release").join("ls");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as ls ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_args_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "args",
            "--bin",
            "args",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("args");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as args ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_cat_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "cat",
            "--bin",
            "cat",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root.join("target/aarch64-unknown-none/release").join("cat");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as cat ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_wc_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "wc",
            "--bin",
            "wc",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root.join("target/aarch64-unknown-none/release").join("wc");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as wc ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_threads_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "threads",
            "--bin",
            "threads",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;
    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("threads");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as threads ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_lua_repl_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "lua-repl",
            "--bin",
            "lua-repl",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("lua-repl");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as lua-repl ELF: {err}", source_path.display()))?;
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

fn build_drv_fb_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "drv-fb",
            "--bin",
            "drv-fb",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("drv-fb");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as drv-fb ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_drv_i2c_hid_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "drv-i2c-hid",
            "--bin",
            "drv-i2c-hid",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("drv-i2c-hid");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes).map_err(|err| {
        format!(
            "validate {} as drv-i2c-hid ELF: {err}",
            source_path.display()
        )
    })?;
    Ok(bytes)
}

fn build_drv_xhci_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "drv-xhci",
            "--bin",
            "drv-xhci",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("drv-xhci");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as drv-xhci ELF: {err}", source_path.display()))?;
    Ok(bytes)
}

fn build_drv_blk_image(root: &Path) -> Result<Vec<u8>, String> {
    run_cargo(
        root,
        &[
            "build",
            "-p",
            "drv-blk",
            "--bin",
            "drv-blk",
            "--target",
            "aarch64-unknown-none",
            "--release",
        ],
    )?;

    let source_path = root
        .join("target/aarch64-unknown-none/release")
        .join("drv-blk");
    let bytes =
        fs::read(&source_path).map_err(|err| format!("read {}: {err}", source_path.display()))?;
    validate_aarch64_kernel_elf(&bytes)
        .map_err(|err| format!("validate {} as drv-blk ELF: {err}", source_path.display()))?;
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
        None,
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

        // Fail the smoke if Sora faulted at runtime. The kernel masks
        // `SoraExhausted` by falling back to the EL0 payload, so `READY`
        // alone is false confidence (FORECAST/001 §5.1).
        let transcript_str = String::from_utf8_lossy(&transcript);
        if transcript_str.contains("*** EL0 FAULT")
            || transcript_str.contains("SoraExhausted")
            || transcript_str.contains("payload fallback")
        {
            return Err(format!(
                "Sora EL0 fault detected in smoke transcript:\n{transcript_str}"
            ));
        }

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

fn run_x86_qemu_smoke(root: &Path) -> Result<(), String> {
    if !command_exists("qemu-system-x86_64") {
        return Err("qemu-system-x86_64 is required for the x86 smoke".to_owned());
    }

    let build_script = root.join("scripts/x86-multiboot.sh");
    run_tool(root, "bash", &[path_arg(&build_script)?, "build"])?;

    let kernel = root.join("target/x86_64-unknown-none/release/kumo-kernel.bin");
    let kernel_len = fs::metadata(&kernel)
        .map_err(|err| format!("metadata {}: {err}", kernel.display()))?
        .len();
    if kernel_len == 0 {
        return Err(format!("{} is empty", kernel.display()));
    }
    let initrd = root.join(X86_MULTIBOOT_INITRD_PATH);
    let initrd_bytes = fs::read(&initrd)
        .map_err(|err| format!("read x86 Multiboot initrd {}: {err}", initrd.display()))?;
    let hello = kumo_abi::find_file(&initrd_bytes, HELLO_PATH)
        .map_err(|err| format!("validate x86 Multiboot initrd: {err:?}"))?
        .ok_or_else(|| format!("{} does not contain {HELLO_PATH}", initrd.display()))?;
    validate_x86_64_kernel_elf(hello.bytes)
        .map_err(|err| format!("validate {} from x86 initrd: {err}", HELLO_PATH))?;

    let mut child = Command::new("qemu-system-x86_64")
        .args([
            "-kernel",
            path_arg(&kernel)?,
            "-initrd",
            path_arg(&initrd)?,
            "-cpu",
            "max,+x2apic",
            "-m",
            "1088",
            "-display",
            "none",
            "-no-reboot",
            "-serial",
            "stdio",
            "-monitor",
            "none",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn qemu-system-x86_64 x86 smoke: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "x86 smoke stdout unavailable".to_owned())?;
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

    let mut transcript = Vec::new();
    let smoke_result = read_serial_until(
        &rx,
        &mut transcript,
        "x86 first-light proof",
        &[
            b"[MUREX] KUMO x86_64 first light (Multiboot/GRUB)",
            b"MULTIBOOT INITRD   Check     1 module  KUMORD01",
            b"bin/hello",
            b"MULTIBOOT BOOTINFO Check     ABIv3",
            b"phys<512G   OK",
            b"M1 MEMORY PLAN     Check",
            b"kernel+initrd excluded",
            b"KERNEL CR3 / PHYSMAP Check",
            b"RAM WB  holes UC/NX  high RAM yes",
            b"GDT / TSS          Check     kernel 0x08/0x10  user 0x23/0x1b  TR 0x28",
            b"ACPI TABLES        Check     RSDP",
            b"ACPI MADT          Check     APIC",
            b"ACPI IRQ ROUTE     Check     ISA IRQ 0 -> GSI 2  IOAPIC 0xfec00000 base 0  high edge  override candidate   OK",
            b"IOAPIC HW          Check     id 0  ver 0x20  entries 24  GSI 0-23 contains 2   OK",
            b"IOAPIC INPUT       Check     GSI 2 pin 2  vec 0x00 fixed physical dest 0  high edge masked idle rirr 0   OK",
            b"IOAPIC PLAN        Check     GSI 2 pin 2  vec 0x31 fixed physical dest 0  high edge masked  raw 0x00000000:0x00010031   OK",
            b"IOAPIC WRITE       Check     GSI 2 pin 2  wrote 0x00000000:0x00010031  readback 0x00000000:0x00010031 masked   OK",
            b"IDT / TOWER        Check     int3 caught + resumed",
            // FP/SIMD boundary (j455): CR0/CR4 values are CPU-dependent; match the OK-only
            // "SSE on" prefix (the FAIL branch has no such text). — KESTREL
            b"FPSIMD / SSE       Check     SSE on",
            b"FPSIMD / XSAVE     Check     AVX on  XCR0 0x7  standard 832b image   OK",
            b"RING3 / FRAMES     Check",
            b"RING3 / PAGING     Check     private CR3  RX code 0x8000000000  NX stack 0x10000000000  4K guard   OK",
            b"RING3 / INT80      Check     CPL3 entered  2 calls  ping 0x4b554d4fc0decafe  exit 0   OK",
            b"FPSIMD / SWITCH   Check     CPL3 2 contexts  4 int80  private CR3",
            b"distinct ymm[255:128] survived   OK",
            b"PIC / PIT          Check     1193182 Hz input  20 Hz tick  IRQ 0  hb 3t   OK",
            b"x2APIC / TIMER    Check",
            b"20 Hz tick  vec 48  hb 3t   OK",
            b"IOAPIC DISPATCH    Check     vec 0x31 software probe counted + EOI  seen 1   OK",
            b"IOAPIC TIMER       Check     PIC IRQ0 masked  GSI 2 vec 0x31 unmasked  hb 3t via I/O APIC   OK",
            b"TIMER SOURCE       Check     local APIC vec 0x30 canonical  I/O APIC route re-masked  hb 3t  ioapic +0   OK",
            b"CONTEXT SWITCH     Check     2 kthreads  16 switches  work 6  callee-saved + stack resume   OK",
            b"PREEMPT SCHED      Check     2 kthreads",
            b"timer-preempted both bodies   OK",
            b"hello from a native KUMO program!",
            b"USER ELF / FRAMES  Check",
            b"USER ELF / ENGINE  Check     2 PT_LOAD  entry 0x8000000000  boot h1  4 int80",
            b"wrote 34b  2 switches  exit 0   OK",
            b"x86_64 MUREX core online, first light reached; HALTING.",
        ],
        Duration::from_secs(8),
    )
    .and_then(|()| validate_x86_smoke_transcript(&transcript));

    stop_qemu_child(&mut child);

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    smoke_result.map_err(|err| {
        if stderr.is_empty() {
            err
        } else {
            format!("{err}\nqemu stderr:\n{stderr}")
        }
    })?;

    println!(
        "KUMO x86 QEMU smoke green: full Multiboot physical map + kernel-owned CR3/high-RAM probe, shared frame allocation + initrd native ELF on shared SyscallEngine, scheduled CPL3/int80, real cooperative/timer-preempted contexts, private process CR3, CPUID-gated AVX/XCR0 plus eager XSAVE ownership across two CPL3 contexts, int3 + interrupt-transparent FP/SIMD state, and the PIC/PIT/x2APIC/I/O APIC chain all proven"
    );
    Ok(())
}

fn run_x86_uefi_smoke(root: &Path) -> Result<(), String> {
    for program in ["qemu-system-x86_64", "mformat", "xorriso"] {
        if !command_exists(program) {
            return Err(format!("{program} is required for the x86 UEFI smoke"));
        }
    }
    if !command_exists("x86_64-elf-grub-mkrescue") && !command_exists("grub-mkrescue") {
        return Err(
            "x86_64-elf-grub-mkrescue or grub-mkrescue is required for the x86 UEFI smoke"
                .to_owned(),
        );
    }

    let (ovmf_code, ovmf_vars_template) = find_ovmf_firmware()?;
    let out_dir = root.join("target/x86-uefi-smoke");
    fs::create_dir_all(&out_dir).map_err(|err| format!("create {}: {err}", out_dir.display()))?;
    let iso = out_dir.join("kumo-amd64.iso");
    let ovmf_vars = out_dir.join("OVMF_VARS.fd");
    fs::copy(&ovmf_vars_template, &ovmf_vars).map_err(|err| {
        format!(
            "copy OVMF variables {} to {}: {err}",
            ovmf_vars_template.display(),
            ovmf_vars.display()
        )
    })?;

    let mkiso = root.join("scripts/mkiso.sh");
    run_tool(root, "bash", &[path_arg(&mkiso)?, "amd64", path_arg(&iso)?])?;
    if fs::metadata(&iso)
        .map_err(|err| format!("metadata {}: {err}", iso.display()))?
        .len()
        == 0
    {
        return Err(format!("{} is empty", iso.display()));
    }

    let code_drive = format!(
        "if=pflash,unit=0,format=raw,readonly=on,file={}",
        ovmf_code.display()
    );
    let vars_drive = format!("if=pflash,unit=1,format=raw,file={}", ovmf_vars.display());
    let cdrom_drive = format!(
        "media=cdrom,format=raw,readonly=on,file.locking=off,file.filename={}",
        iso.display()
    );
    let mut child = Command::new("qemu-system-x86_64")
        .args(["-machine", "q35", "-drive"])
        .arg(&code_drive)
        .args(["-drive"])
        .arg(&vars_drive)
        .args(["-drive"])
        .arg(&cdrom_drive)
        .args([
            "-boot",
            "d",
            "-cpu",
            "max,+x2apic",
            "-m",
            "1088",
            "-display",
            "none",
            "-serial",
            "stdio",
            "-monitor",
            "none",
            "-no-reboot",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawn qemu-system-x86_64 UEFI smoke: {err}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "x86 UEFI smoke stdout unavailable".to_owned())?;
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

    let mut transcript = Vec::new();
    let reached_first_light = read_serial_until(
        &rx,
        &mut transcript,
        "x86 UEFI/GRUB first-light proof",
        &[b"x86_64 MUREX core online, first light reached; HALTING."],
        Duration::from_secs(30),
    );
    let smoke_result = match reached_first_light {
        Ok(()) => validate_x86_uefi_smoke_transcript(&transcript),
        Err(err) => Err(format!(
            "{err}\nx86 UEFI transcript:\n{}",
            String::from_utf8_lossy(&transcript)
        )),
    };

    stop_qemu_child(&mut child);

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    smoke_result.map_err(|err| {
        if stderr.is_empty() {
            err
        } else {
            format!("{err}\nqemu stderr:\n{stderr}")
        }
    })?;

    println!(
        "KUMO x86 UEFI smoke green: OVMF -> GRUB -> Multiboot2 supplied the initrd, full firmware memory map, and ACPI RSDP; first light completed"
    );
    Ok(())
}

fn find_ovmf_firmware() -> Result<(PathBuf, PathBuf), String> {
    let code_override = env::var_os("KUMO_OVMF_CODE").map(PathBuf::from);
    let vars_override = env::var_os("KUMO_OVMF_VARS").map(PathBuf::from);
    match (code_override, vars_override) {
        (Some(code), Some(vars)) => {
            if !code.is_file() {
                return Err(format!("KUMO_OVMF_CODE is not a file: {}", code.display()));
            }
            if !vars.is_file() {
                return Err(format!("KUMO_OVMF_VARS is not a file: {}", vars.display()));
            }
            return Ok((code, vars));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(
                "set both KUMO_OVMF_CODE and KUMO_OVMF_VARS when overriding OVMF discovery"
                    .to_owned(),
            );
        }
        (None, None) => {}
    }

    for (code, vars) in [
        (
            "/opt/homebrew/share/qemu/edk2-x86_64-code.fd",
            "/opt/homebrew/share/qemu/edk2-i386-vars.fd",
        ),
        (
            "/usr/local/share/qemu/edk2-x86_64-code.fd",
            "/usr/local/share/qemu/edk2-i386-vars.fd",
        ),
        (
            "/usr/share/qemu/edk2-x86_64-code.fd",
            "/usr/share/qemu/edk2-i386-vars.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/edk2/x64/OVMF_CODE.fd",
            "/usr/share/edk2/x64/OVMF_VARS.fd",
        ),
        (
            "/usr/share/edk2/ovmf/OVMF_CODE.fd",
            "/usr/share/edk2/ovmf/OVMF_VARS.fd",
        ),
    ] {
        let code = PathBuf::from(code);
        let vars = PathBuf::from(vars);
        if code.is_file() && vars.is_file() {
            return Ok((code, vars));
        }
    }

    Err(
        "could not find a matching OVMF code/variables pair; set KUMO_OVMF_CODE and KUMO_OVMF_VARS"
            .to_owned(),
    )
}

fn validate_x86_uefi_smoke_transcript(transcript: &[u8]) -> Result<(), String> {
    validate_x86_smoke_transcript(transcript)?;
    let text = String::from_utf8_lossy(transcript);
    for marker in [
        "GNU GRUB",
        "multiboot: v2 magic=0x36d76289",
        "multiboot2:",
        "via Multiboot2   OK",
    ] {
        if !text.contains(marker) {
            return Err(format!(
                "x86 UEFI smoke transcript missing {marker:?}:\n{text}"
            ));
        }
    }
    Ok(())
}

fn validate_x86_smoke_transcript(transcript: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(transcript);
    for marker in [
        "[MUREX] KUMO x86_64 first light (Multiboot/GRUB)",
        "MULTIBOOT INITRD   Check     1 module  KUMORD01",
        "bin/hello",
        "MULTIBOOT BOOTINFO Check     ABIv3",
        "phys<512G   OK",
        "M1 MEMORY PLAN     Check",
        "kernel+initrd excluded",
        "KERNEL CR3 / PHYSMAP Check",
        "RAM WB  holes UC/NX  high RAM yes",
        "GDT / TSS          Check     kernel 0x08/0x10  user 0x23/0x1b  TR 0x28",
        "ACPI TABLES        Check     RSDP",
        "ACPI MADT          Check     APIC",
        "ACPI IRQ ROUTE     Check     ISA IRQ 0 -> GSI 2  IOAPIC 0xfec00000 base 0  high edge  override candidate   OK",
        "IOAPIC HW          Check     id 0  ver 0x20  entries 24  GSI 0-23 contains 2   OK",
        "IOAPIC INPUT       Check     GSI 2 pin 2  vec 0x00 fixed physical dest 0  high edge masked idle rirr 0   OK",
        "IOAPIC PLAN        Check     GSI 2 pin 2  vec 0x31 fixed physical dest 0  high edge masked  raw 0x00000000:0x00010031   OK",
        "IOAPIC WRITE       Check     GSI 2 pin 2  wrote 0x00000000:0x00010031  readback 0x00000000:0x00010031 masked   OK",
        "IDT / TOWER        Check     int3 caught + resumed",
        "FPSIMD / SSE       Check     SSE on",
        "FPSIMD / XSAVE     Check     AVX on  XCR0 0x7  standard 832b image   OK",
        "RING3 / FRAMES     Check",
        "RING3 / PAGING     Check     private CR3  RX code 0x8000000000  NX stack 0x10000000000  4K guard   OK",
        "RING3 / INT80      Check     CPL3 entered  2 calls  ping 0x4b554d4fc0decafe  exit 0   OK",
        "FPSIMD / SWITCH   Check     CPL3 2 contexts  4 int80  private CR3",
        "distinct ymm[255:128] survived   OK",
        "PIC / PIT          Check     1193182 Hz input  20 Hz tick  IRQ 0  hb 3t   OK",
        "x2APIC / TIMER    Check",
        "20 Hz tick  vec 48  hb 3t   OK",
        "IOAPIC DISPATCH    Check     vec 0x31 software probe counted + EOI  seen 1   OK",
        "IOAPIC TIMER       Check     PIC IRQ0 masked  GSI 2 vec 0x31 unmasked  hb 3t via I/O APIC   OK",
        "TIMER SOURCE       Check     local APIC vec 0x30 canonical  I/O APIC route re-masked  hb 3t  ioapic +0   OK",
        "CONTEXT SWITCH     Check     2 kthreads  16 switches  work 6  callee-saved + stack resume   OK",
        "PREEMPT SCHED      Check     2 kthreads",
        "timer-preempted both bodies   OK",
        "hello from a native KUMO program!",
        "USER ELF / FRAMES  Check",
        "USER ELF / ENGINE  Check     2 PT_LOAD  entry 0x8000000000  boot h1  4 int80",
        "wrote 34b  2 switches  exit 0   OK",
        "x86_64 MUREX core online, first light reached; HALTING.",
    ] {
        if !text.contains(marker) {
            return Err(format!("x86 smoke transcript missing {marker:?}:\n{text}"));
        }
    }
    if text.contains("FAIL") || text.contains("TOWER-x86: fatal exception") {
        return Err(format!("x86 smoke transcript contains a failure:\n{text}"));
    }
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

#[cfg(test)]
mod boot_manifest_tests {
    use super::{
        esp_uefi_path, parse_pl011_console_arg, render_boot_manifest, BOOT_MANIFEST_ESP_PATH,
    };
    use imager::{HardwareTarget, ImagePlan};
    use niji_uefi::{BootConfig, DtbSource};

    fn manifest_for(hardware: HardwareTarget) -> String {
        render_boot_manifest(&ImagePlan::new("", hardware), None)
    }

    /// The whole point of the manifest: what xtask writes, Nijigumo's own parser reads back,
    /// and the id it yields resolves to the same `kumo_bsp::Board` the imager built for. If
    /// producer and consumer ever drift, this fails rather than the board going unstamped on
    /// metal (DESIGN/017 §4 step 2).
    #[test]
    fn every_aarch64_target_round_trips_producer_to_consumer_to_board() {
        for hardware in [
            HardwareTarget::QemuVirtAarch64,
            HardwareTarget::ThinkPadX13sGen1,
            HardwareTarget::RaspberryPi5,
            HardwareTarget::OrangePi5Plus,
        ] {
            let text = manifest_for(hardware);
            let cfg = BootConfig::parse(&text).expect("manifest must parse");
            let id = cfg.board.expect("aarch64 target must stamp a board");
            assert_eq!(kumo_bsp::Board::from_id(id), hardware.board());
        }
    }

    /// x86_64 has no aarch64 BSP entry, so it stamps nothing and the kernel reports the board
    /// unstamped — but the manifest must still be a valid, parseable config.
    #[test]
    fn x86_64_target_stamps_no_board_but_still_parses() {
        let text = manifest_for(HardwareTarget::GenericUefiX86_64);
        let cfg = BootConfig::parse(&text).expect("manifest must parse");
        assert_eq!(cfg.board, None);
    }

    /// The loader opens these with UEFI's file protocol, which wants absolute, backslash
    /// paths — not the `/`-separated form the imager models.
    #[test]
    fn manifest_names_files_in_uefi_path_form() {
        assert_eq!(
            esp_uefi_path("EFI/KUMO/kernel/kumo-kernel.elf"),
            "\\EFI\\KUMO\\kernel\\kumo-kernel.elf"
        );
        let cfg_text = manifest_for(HardwareTarget::QemuVirtAarch64);
        let cfg = BootConfig::parse(&cfg_text).unwrap();
        assert_eq!(cfg.kernel, "\\EFI\\KUMO\\kernel\\kumo-kernel.elf");
        assert_eq!(cfg.initrd, Some("\\EFI\\KUMO\\initrd.img"));
        // And the manifest itself is staged where the loader looks for it.
        assert_eq!(
            esp_uefi_path(BOOT_MANIFEST_ESP_PATH),
            "\\EFI\\KUMO\\nijigumo.conf"
        );
    }

    /// A board that ships its own DTB gets an `esp:` override; one that trusts firmware gets
    /// the universal handoff. This is the existing `DtbSource` contract — asserted here so
    /// adding `board` to the manifest cannot silently change what the loader would load.
    #[test]
    fn dtb_source_matches_whether_the_target_stages_a_dtb() {
        let x13s = manifest_for(HardwareTarget::ThinkPadX13sGen1);
        assert_eq!(
            BootConfig::parse(&x13s).unwrap().dtb,
            DtbSource::Esp("\\EFI\\KUMO\\dtb\\qcom\\sc8280xp-lenovo-thinkpad-x13s.dtb")
        );
        // The Pi 5 has no staged DTB: EDK2 publishes one via the firmware handoff.
        let pi5 = manifest_for(HardwareTarget::RaspberryPi5);
        assert_eq!(BootConfig::parse(&pi5).unwrap().dtb, DtbSource::Firmware);
    }

    #[test]
    fn explicit_rp1_uart_route_round_trips_into_the_loader_parser() {
        let base = parse_pl011_console_arg("pl011@0x1c00030000").unwrap();
        let text = render_boot_manifest(
            &ImagePlan::new("", HardwareTarget::RaspberryPi5),
            Some(base),
        );
        let cfg = BootConfig::parse(&text).unwrap();
        assert_eq!(
            cfg.console_uart.and_then(niji_uefi::parse_pl011_console),
            Some(0x1c_0003_0000)
        );
    }

    #[test]
    fn console_uart_cli_rejects_sentinels_and_misalignment() {
        assert!(parse_pl011_console_arg("dw8250@0x1c00030000").is_err());
        assert!(parse_pl011_console_arg("pl011@0xffffffffffffffff").is_err());
        assert!(parse_pl011_console_arg("pl011@0x1c00030001").is_err());
    }
}

#[cfg(test)]
mod fat32_image_tests {
    use super::build_fat32_image;
    use kumo_fatfs::{FatVolume, SectorReader, SECTOR_SIZE};

    /// A `SectorReader` over the in-memory image `build_fat32_image` produces, so
    /// the host test exercises the exact bytes the initrd ships, with the same
    /// reader that runs on target.
    struct ImageDisk(Vec<u8>);
    impl SectorReader for ImageDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            let off = lba as usize * SECTOR_SIZE;
            match self.0.get(off..off + SECTOR_SIZE) {
                Some(s) => {
                    buf.copy_from_slice(s);
                    true
                }
                None => false,
            }
        }
    }

    #[test]
    fn generated_image_resolves_root_and_esp_paths() {
        let mut disk = ImageDisk(build_fat32_image());
        let vol = FatVolume::mount(&mut disk).expect("mount generated fat32.img");

        // The existing root file still resolves (regression guard).
        let hello = vol
            .resolve_path(&mut disk, b"/HELLO.TXT")
            .expect("/HELLO.TXT");
        let mut out = [0u8; 16];
        let n = vol.read_file(&mut disk, &hello, &mut out);
        assert_eq!(&out[..n], b"hello!");

        // The new subtree: descend EFI/BOOT and read the boot-loader-shaped file.
        let app = vol
            .resolve_path(&mut disk, b"/EFI/BOOT/BOOTAA64.EFI")
            .expect("/EFI/BOOT/BOOTAA64.EFI");
        assert!(!app.is_dir());
        let mut out = [0u8; 16];
        let n = vol.read_file(&mut disk, &app, &mut out);
        assert_eq!(&out[..n], b"esp-ok");

        // Case-insensitive, leading-slash-optional lookup hits the same entry.
        assert_eq!(
            vol.resolve_path(&mut disk, b"efi/boot/bootaa64.efi"),
            Some(app)
        );
    }
}

#[cfg(test)]
mod x86_smoke_tests {
    use super::{validate_x86_smoke_transcript, validate_x86_uefi_smoke_transcript};

    const GREEN: &str = "[MUREX] KUMO x86_64 first light (Multiboot/GRUB)\n\
MULTIBOOT INITRD   Check     1 module  KUMORD01 9000b  bin/hello 8904b  phys 0x938000   OK\n\
MULTIBOOT BOOTINFO Check     ABIv3  4 regions  1086 MiB usable / 1087 MiB mapped  kernel 0x100000+8411 KiB  initrd 0x938000+9000b  phys<512G   OK\n\
M1 MEMORY PLAN     Check     276134 frames / 1078 MiB  kernel+initrd excluded  samples 0x937000 0x93b000 0x93c000   OK\n\
KERNEL CR3 / PHYSMAP Check     old 0x930000 new 0x937000  6 tables  4 GiB  6 low BootInfo frames  RAM WB  holes UC/NX  high RAM yes 0x40000000   OK\n\
GDT / TSS          Check     kernel 0x08/0x10  user 0x23/0x1b  TR 0x28  rsp0 0x132000   OK\n\
ACPI TABLES        Check     RSDP 0x000f59d0 rev 2  XSDT 0x07fe1e98   OK\n\
ACPI MADT          Check     APIC 0x07fe2100  LAPIC 0xfee00000  IOAPIC 1  ISO 0  PCAT true   OK\n\
ACPI IRQ ROUTE     Check     ISA IRQ 0 -> GSI 2  IOAPIC 0xfec00000 base 0  high edge  override candidate   OK\n\
IOAPIC HW          Check     id 0  ver 0x20  entries 24  GSI 0-23 contains 2   OK\n\
IOAPIC INPUT       Check     GSI 2 pin 2  vec 0x00 fixed physical dest 0  high edge masked idle rirr 0   OK\n\
IOAPIC PLAN        Check     GSI 2 pin 2  vec 0x31 fixed physical dest 0  high edge masked  raw 0x00000000:0x00010031   OK\n\
IOAPIC WRITE       Check     GSI 2 pin 2  wrote 0x00000000:0x00010031  readback 0x00000000:0x00010031 masked   OK\n\
IDT / TOWER        Check     int3 caught + resumed  seen 1   OK\n\
FPSIMD / SSE       Check     SSE on (CR0 0x80000013 CR4 0x620)  xmm 0xf00d5555aaaac0de survived int3 ISR   OK\n\
FPSIMD / XSAVE     Check     AVX on  XCR0 0x7  standard 832b image   OK\n\
RING3 / FRAMES     Check     8 BootInfo frames  first 0x937000  last 0x941000  monotonic  kernel+initrd excluded   OK\n\
RING3 / PAGING     Check     private CR3  RX code 0x8000000000  NX stack 0x10000000000  4K guard   OK\n\
RING3 / INT80      Check     CPL3 entered  2 calls  ping 0x4b554d4fc0decafe  exit 0   OK\n\
FPSIMD / SWITCH   Check     CPL3 2 contexts  4 int80  private CR3 0x942000/0x94a000  distinct ymm[255:128] survived   OK\n\
PIC / PIT          Check     1193182 Hz input  20 Hz tick  IRQ 0  hb 3t   OK\n\
x2APIC / TIMER    Check     50000000 Hz calibrated  20 Hz tick  vec 48  hb 3t   OK\n\
IOAPIC DISPATCH    Check     vec 0x31 software probe counted + EOI  seen 1   OK\n\
IOAPIC TIMER       Check     PIC IRQ0 masked  GSI 2 vec 0x31 unmasked  hb 3t via I/O APIC   OK\n\
TIMER SOURCE       Check     local APIC vec 0x30 canonical  I/O APIC route re-masked  hb 3t  ioapic +0   OK\n\
CONTEXT SWITCH     Check     2 kthreads  16 switches  work 6  callee-saved + stack resume   OK\n\
PREEMPT SCHED      Check     2 kthreads  4 body switches  5 ticks  timer-preempted both bodies   OK\n\
hello from a native KUMO program!\n\
USER ELF / FRAMES  Check     24 BootInfo frames  first 0x942000  last 0x959000  CR3 0x942000  monotonic  kernel+initrd excluded   OK\n\
USER ELF / ENGINE  Check     2 PT_LOAD  entry 0x8000000000  boot h1  4 int80  wrote 34b  2 switches  exit 0   OK\n\
x86_64 MUREX core online, first light reached; HALTING.\n";

    #[test]
    fn x86_transcript_requires_all_live_interrupt_proofs() {
        assert_eq!(validate_x86_smoke_transcript(GREEN.as_bytes()), Ok(()));
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("KERNEL CR3 / PHYSMAP Check", "TRAMPOLINE CR3       Check")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("RING3 / FRAMES     Check", "RING3 / STATIC     Check")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("FPSIMD / SWITCH   Check", "FPSIMD / SHARED   Check")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("distinct ymm[255:128] survived", "distinct xmm survived")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("USER ELF / FRAMES  Check", "USER ELF / STATIC  Check")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("IRQ 0  hb 3t   OK", "IRQ 0  hb 2t   OK")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_smoke_transcript(
            GREEN
                .replace("vec 48  hb 3t   OK", "vec 48  hb 2t   OK")
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn x86_transcript_rejects_a_failure_even_after_first_light() {
        let transcript = format!("{GREEN}TOWER-x86: fatal exception; HALT\n");
        assert!(validate_x86_smoke_transcript(transcript.as_bytes()).is_err());
    }

    #[test]
    fn x86_uefi_transcript_requires_grub_multiboot2_and_tagged_acpi() {
        let transcript = format!(
            "GNU GRUB  version 2.12\nmultiboot: v2 magic=0x36d76289 info@0x4000\nmultiboot2: 5816b tagged handoff\n{}",
            GREEN.replace(
                "XSDT 0x07fe1e98   OK",
                "XSDT 0x07fe1e98  via Multiboot2   OK"
            )
        );
        assert_eq!(
            validate_x86_uefi_smoke_transcript(transcript.as_bytes()),
            Ok(())
        );
        assert!(validate_x86_uefi_smoke_transcript(
            transcript
                .replace("v2 magic=0x36d76289", "v1 magic=0x2badb002")
                .as_bytes()
        )
        .is_err());
        assert!(validate_x86_uefi_smoke_transcript(
            transcript
                .replace("via Multiboot2", "legacy scan")
                .as_bytes()
        )
        .is_err());
    }
}

fn print_help() {
    println!(
        "usage: cargo xtask <build|test|boot-files|qemu-smoke|x86-smoke|x86-uefi-smoke|x86-initrd|image|product|run|preflight> [--arch aarch64|x86_64] [--hardware x13s|qemu-virt-aarch64|rpi5|opi5plus|generic-uefi-x86_64] [--console-uart pl011@0x<base>]"
    );
    println!("default arch: aarch64; default hardware: thinkpad-x13s-gen1");
    println!("--console-uart: image-only explicit PL011 route; use firmware's reported MMIO base");
    println!("x86-uefi-smoke: exact OVMF -> GRUB -> Multiboot2 ISO path; KUMO_OVMF_CODE/KUMO_OVMF_VARS override firmware discovery");
    println!("preflight: mechanical guardrail tripwires (GUIDANCE/006 §5); KUMO_PREFLIGHT_FULL=1 adds both-backend build + smoke");
}
