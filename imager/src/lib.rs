use std::fmt;
use std::path::{Path, PathBuf};

pub const X13S_DTB_SOURCE_PATH: &str = "sc8280xp-lenovo-thinkpad-x13s.dtb";
pub const X13S_DTB_ESP_PATH: &str = "EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb";
pub const X13S_DTB_COMPATIBLES: &[&str] = &["lenovo,thinkpad-x13s", "qcom,sc8280xp"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageArch {
    Aarch64,
    X86_64,
}

impl ImageArch {
    pub fn efi_boot_name(self) -> &'static str {
        match self {
            Self::Aarch64 => "BOOTAA64.EFI",
            Self::X86_64 => "BOOTX64.EFI",
        }
    }
}

impl fmt::Display for ImageArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aarch64 => f.write_str("aarch64"),
            Self::X86_64 => f.write_str("x86_64"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardwareTarget {
    ThinkPadX13sGen1,
    QemuVirtAarch64,
    GenericUefiX86_64,
}

impl HardwareTarget {
    pub fn default_for_arch(arch: ImageArch) -> Self {
        match arch {
            ImageArch::Aarch64 => Self::ThinkPadX13sGen1,
            ImageArch::X86_64 => Self::GenericUefiX86_64,
        }
    }

    pub fn profile(self) -> HardwareProfile {
        match self {
            Self::ThinkPadX13sGen1 => HardwareProfile {
                id: "thinkpad-x13s-gen1",
                name: "Lenovo ThinkPad X13s Gen 1",
                arch: ImageArch::Aarch64,
                soc: "Qualcomm Snapdragon 8cx Gen 3 / SC8280XP",
                firmware: "UEFI",
                interrupt_controller: "GICv3",
                early_console: "UEFI GOP/console first; exposed UART unknown",
                dtb_source_path: Some(X13S_DTB_SOURCE_PATH),
                dtb_path: Some(X13S_DTB_ESP_PATH),
                dtb_compatibles: X13S_DTB_COMPATIBLES,
                firmware_notes: &[
                    "update firmware before bring-up",
                    "enable Linux Boot in firmware",
                    "disable Secure Boot until Nijigumo is signed",
                ],
            },
            Self::QemuVirtAarch64 => HardwareProfile {
                id: "qemu-virt-aarch64",
                name: "QEMU virt aarch64",
                arch: ImageArch::Aarch64,
                soc: "QEMU virt",
                firmware: "UEFI/AAVMF",
                interrupt_controller: "GICv3 model",
                early_console: "PL011 UART0 at 0x09000000; GOP when ramfb is present",
                dtb_source_path: None,
                dtb_path: None,
                dtb_compatibles: &[],
                firmware_notes: &[
                    "test target, not physical hardware",
                    "run QEMU with gic-version=3",
                ],
            },
            Self::GenericUefiX86_64 => HardwareProfile {
                id: "generic-uefi-x86_64",
                name: "Generic x86_64 UEFI",
                arch: ImageArch::X86_64,
                soc: "x86_64 PC",
                firmware: "UEFI/OVMF",
                interrupt_controller: "APIC/x2APIC",
                early_console: "UEFI GOP/console first; serial optional",
                dtb_source_path: None,
                dtb_path: None,
                dtb_compatibles: &[],
                firmware_notes: &["x86_64 metal remains later; keep CI green"],
            },
        }
    }
}

impl fmt::Display for HardwareTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.profile().id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HardwareProfile {
    pub id: &'static str,
    pub name: &'static str,
    pub arch: ImageArch,
    pub soc: &'static str,
    pub firmware: &'static str,
    pub interrupt_controller: &'static str,
    pub early_console: &'static str,
    pub dtb_source_path: Option<&'static str>,
    pub dtb_path: Option<&'static str>,
    pub dtb_compatibles: &'static [&'static str],
    pub firmware_notes: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImagePlan {
    pub hardware: HardwareTarget,
    pub arch: ImageArch,
    pub esp_boot_path: PathBuf,
    pub kernel_path: PathBuf,
    pub initrd_path: PathBuf,
    pub dtb_source_path: Option<PathBuf>,
    pub dtb_path: Option<PathBuf>,
    pub dtb_compatibles: Vec<&'static str>,
    pub firmware_notes: Vec<&'static str>,
}

impl ImagePlan {
    pub fn for_arch(root: impl AsRef<Path>, arch: ImageArch) -> Self {
        Self::new(root, HardwareTarget::default_for_arch(arch))
    }

    pub fn new(root: impl AsRef<Path>, hardware: HardwareTarget) -> Self {
        let root = root.as_ref();
        let profile = hardware.profile();
        Self {
            hardware,
            arch: profile.arch,
            esp_boot_path: PathBuf::from(r"EFI/BOOT").join(profile.arch.efi_boot_name()),
            kernel_path: root.join("build/kernel/kumo.elf"),
            initrd_path: root.join("build/initrd/kumo-initrd.tar"),
            dtb_source_path: profile.dtb_source_path.map(|path| root.join(path)),
            dtb_path: profile.dtb_path.map(PathBuf::from),
            dtb_compatibles: profile.dtb_compatibles.to_vec(),
            firmware_notes: profile.firmware_notes.to_vec(),
        }
    }

    pub fn manifest(&self) -> String {
        let profile = self.hardware.profile();
        let mut manifest = format!(
            "hardware={}\nhardware_name={}\narch={}\nsoc={}\nfirmware={}\ninterrupt_controller={}\nearly_console={}\nesp_boot_path={}\nkernel_path={}\ninitrd_path={}\n",
            self.hardware,
            profile.name,
            self.arch,
            profile.soc,
            profile.firmware,
            profile.interrupt_controller,
            profile.early_console,
            self.esp_boot_path.display(),
            self.kernel_path.display(),
            self.initrd_path.display()
        );

        if let Some(path) = &self.dtb_source_path {
            manifest.push_str(&format!("dtb_source_path={}\n", path.display()));
        }
        if let Some(path) = &self.dtb_path {
            manifest.push_str(&format!("dtb_path={}\n", path.display()));
        }
        if !self.dtb_compatibles.is_empty() {
            manifest.push_str("dtb_compatible=");
            for (index, compatible) in self.dtb_compatibles.iter().enumerate() {
                if index != 0 {
                    manifest.push(';');
                }
                manifest.push_str(compatible);
            }
            manifest.push('\n');
        }
        for note in &self.firmware_notes {
            manifest.push_str(&format!("firmware_note={note}\n"));
        }

        manifest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DtbSummary {
    pub total_size: u32,
    pub version: u32,
    pub boot_cpuid_phys: u32,
    pub model: Option<String>,
    pub root_compatibles: Vec<String>,
}

impl DtbSummary {
    pub fn parse(bytes: &[u8]) -> Result<Self, DtbError> {
        const HEADER_LEN: usize = 40;
        const FDT_MAGIC: u32 = 0xd00d_feed;
        const FDT_BEGIN_NODE: u32 = 1;
        const FDT_END_NODE: u32 = 2;
        const FDT_PROP: u32 = 3;
        const FDT_NOP: u32 = 4;
        const FDT_END: u32 = 9;

        if bytes.len() < HEADER_LEN {
            return Err(DtbError::TooSmall {
                actual: bytes.len(),
            });
        }

        let magic = read_be_u32(bytes, 0)?;
        if magic != FDT_MAGIC {
            return Err(DtbError::BadMagic(magic));
        }

        let total_size = read_be_u32(bytes, 4)?;
        let total_size_usize = total_size as usize;
        if total_size_usize > bytes.len() {
            return Err(DtbError::Truncated {
                needed: total_size_usize,
                actual: bytes.len(),
            });
        }

        let off_dt_struct = read_be_u32(bytes, 8)? as usize;
        let off_dt_strings = read_be_u32(bytes, 12)? as usize;
        let version = read_be_u32(bytes, 20)?;
        let boot_cpuid_phys = read_be_u32(bytes, 28)?;
        let size_dt_strings = read_be_u32(bytes, 32)? as usize;
        let size_dt_struct = read_be_u32(bytes, 36)? as usize;
        let struct_end = checked_end(off_dt_struct, size_dt_struct, total_size_usize)?;
        let strings_end = checked_end(off_dt_strings, size_dt_strings, total_size_usize)?;
        let strings = &bytes[off_dt_strings..strings_end];

        let mut cursor = off_dt_struct;
        let token = read_be_u32(bytes, cursor)?;
        cursor = cursor.checked_add(4).ok_or(DtbError::InvalidOffset)?;
        if token != FDT_BEGIN_NODE {
            return Err(DtbError::MissingRootNode);
        }

        let name_len = nul_terminated_len(bytes, cursor, struct_end)?;
        if name_len != 0 {
            return Err(DtbError::MissingRootNode);
        }
        cursor = align4(cursor + name_len + 1).ok_or(DtbError::InvalidOffset)?;

        let mut model = None;
        let mut root_compatibles = Vec::new();

        while cursor < struct_end {
            let token = read_be_u32(bytes, cursor)?;
            cursor = cursor.checked_add(4).ok_or(DtbError::InvalidOffset)?;
            match token {
                FDT_PROP => {
                    let len = read_be_u32(bytes, cursor)? as usize;
                    cursor = cursor.checked_add(4).ok_or(DtbError::InvalidOffset)?;
                    let name_offset = read_be_u32(bytes, cursor)? as usize;
                    cursor = cursor.checked_add(4).ok_or(DtbError::InvalidOffset)?;
                    let data_end = checked_end(cursor, len, struct_end)?;
                    let property_name = read_string(strings, name_offset)?;
                    let data = &bytes[cursor..data_end];
                    if property_name == "model" {
                        model = Some(read_cstr_property(data)?);
                    } else if property_name == "compatible" {
                        root_compatibles = read_cstr_list_property(data)?;
                    }
                    cursor = align4(data_end).ok_or(DtbError::InvalidOffset)?;
                }
                FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => break,
                FDT_NOP => {}
                other => return Err(DtbError::InvalidStructureToken(other)),
            }
        }

        Ok(Self {
            total_size,
            version,
            boot_cpuid_phys,
            model,
            root_compatibles,
        })
    }

    pub fn has_compatible(&self, compatible: &str) -> bool {
        self.root_compatibles
            .iter()
            .any(|candidate| candidate == compatible)
    }

    pub fn has_compatibles(&self, compatibles: &[&str]) -> bool {
        compatibles
            .iter()
            .all(|compatible| self.has_compatible(compatible))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtbError {
    TooSmall { actual: usize },
    BadMagic(u32),
    Truncated { needed: usize, actual: usize },
    InvalidOffset,
    MissingRootNode,
    InvalidStructureToken(u32),
    InvalidString,
}

impl fmt::Display for DtbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { actual } => write!(f, "DTB header too small ({actual} bytes)"),
            Self::BadMagic(magic) => write!(f, "DTB magic is 0x{magic:08x}, expected 0xd00dfeed"),
            Self::Truncated { needed, actual } => {
                write!(f, "DTB declares {needed} bytes but file has {actual}")
            }
            Self::InvalidOffset => f.write_str("DTB contains an invalid offset"),
            Self::MissingRootNode => f.write_str("DTB root node is missing"),
            Self::InvalidStructureToken(token) => {
                write!(f, "DTB contains unexpected structure token {token}")
            }
            Self::InvalidString => f.write_str("DTB contains a non-UTF-8 string"),
        }
    }
}

impl std::error::Error for DtbError {}

fn read_be_u32(bytes: &[u8], offset: usize) -> Result<u32, DtbError> {
    let end = offset.checked_add(4).ok_or(DtbError::InvalidOffset)?;
    if end > bytes.len() {
        return Err(DtbError::Truncated {
            needed: end,
            actual: bytes.len(),
        });
    }

    Ok(u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn checked_end(start: usize, len: usize, limit: usize) -> Result<usize, DtbError> {
    let end = start.checked_add(len).ok_or(DtbError::InvalidOffset)?;
    if end > limit {
        return Err(DtbError::Truncated {
            needed: end,
            actual: limit,
        });
    }
    Ok(end)
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn nul_terminated_len(bytes: &[u8], start: usize, limit: usize) -> Result<usize, DtbError> {
    if start >= limit || limit > bytes.len() {
        return Err(DtbError::InvalidOffset);
    }

    bytes[start..limit]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(DtbError::InvalidOffset)
}

fn read_string(strings: &[u8], offset: usize) -> Result<&str, DtbError> {
    if offset >= strings.len() {
        return Err(DtbError::InvalidOffset);
    }
    let len = strings[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(DtbError::InvalidOffset)?;
    std::str::from_utf8(&strings[offset..offset + len]).map_err(|_| DtbError::InvalidString)
}

fn read_cstr_property(data: &[u8]) -> Result<String, DtbError> {
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len());
    std::str::from_utf8(&data[..end])
        .map(str::to_owned)
        .map_err(|_| DtbError::InvalidString)
}

fn read_cstr_list_property(data: &[u8]) -> Result<Vec<String>, DtbError> {
    let mut strings = Vec::new();
    for raw in data.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        strings.push(
            std::str::from_utf8(raw)
                .map(str::to_owned)
                .map_err(|_| DtbError::InvalidString)?,
        );
    }
    Ok(strings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn arm64_uses_uefi_aa64_boot_name() {
        let plan = ImagePlan::for_arch("/kumo", ImageArch::Aarch64);
        assert_eq!(plan.esp_boot_path, PathBuf::from(r"EFI/BOOT/BOOTAA64.EFI"));
    }

    #[test]
    fn x13s_profile_carries_dtb_contract() {
        let plan = ImagePlan::new("/kumo", HardwareTarget::ThinkPadX13sGen1);
        assert_eq!(plan.arch, ImageArch::Aarch64);
        assert_eq!(
            plan.dtb_source_path,
            Some(PathBuf::from("/kumo").join(X13S_DTB_SOURCE_PATH))
        );
        assert_eq!(plan.dtb_path, Some(PathBuf::from(X13S_DTB_ESP_PATH)));
        assert!(plan
            .manifest()
            .contains("dtb_source_path=/kumo/sc8280xp-lenovo-thinkpad-x13s.dtb"));
        assert!(plan
            .manifest()
            .contains("dtb_compatible=lenovo,thinkpad-x13s;qcom,sc8280xp"));
        assert!(plan
            .manifest()
            .contains("firmware_note=enable Linux Boot in firmware"));
    }

    #[test]
    fn x13s_dtb_matches_hardware_contract() {
        let bytes = fs::read(workspace_root().join(X13S_DTB_SOURCE_PATH)).unwrap();
        let summary = DtbSummary::parse(&bytes).unwrap();
        assert_eq!(summary.version, 17);
        assert_eq!(summary.total_size as usize, bytes.len());
        assert_eq!(summary.model.as_deref(), Some("Lenovo ThinkPad X13s"));
        assert!(summary.has_compatibles(X13S_DTB_COMPATIBLES));
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }
}
