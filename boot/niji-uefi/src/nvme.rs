//j800
//! UEFI NVMe PassThru bindings and reference decoding, independent of drv-nvme.
//! UEFI 2.10 §13.15 and TianoCore MdePkg/Include/Protocol/NvmExpressPassthru.h.
//! Identify only; firmware owns queue transport and DMA mapping. — TIMBERDOODLE

use core::{ffi::c_void, fmt, ptr};
pub const PAGE: usize = 4096;
pub const TIMEOUT_100NS: u64 = 300_000_000;

#[repr(C)]
pub struct Mode {
    pub attributes: u32,
    pub io_align: u32,
    pub version: u32,
}
#[repr(C)]
#[derive(Default)]
pub struct Command {
    pub opcode: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}
#[repr(C)]
pub struct Completion {
    pub words: [u32; 4],
}
#[repr(C)]
pub struct Packet {
    pub timeout: u64,
    pub buffer: *mut c_void,
    pub length: u32,
    pub metadata: *mut c_void,
    pub metadata_length: u32,
    pub queue: u8,
    pub command: *mut Command,
    pub completion: *mut Completion,
}
#[repr(C)]
pub struct Protocol {
    pub mode: *const Mode,
    pub pass_thru: unsafe extern "efiapi" fn(*mut Protocol, u32, *mut Packet, *mut c_void) -> usize,
    pub next_namespace: unsafe extern "efiapi" fn(*mut Protocol, *mut u32) -> usize,
    pub build_device_path: *const c_void,
    pub get_namespace: *const c_void,
}

pub fn eligible(board: &str) -> bool {
    board == "orange-pi-5-plus"
}
pub fn supported(mode: &Mode) -> bool {
    mode.attributes & 1 != 0
        && (mode.io_align <= 1 || (mode.io_align.is_power_of_two() && mode.io_align <= PAGE as u32))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Input,
    Firmware(usize),
    Short(u32),
    Completion(u32),
    Data,
}

/// Caller provides a live protocol and page-aligned writable PAGE-byte allocation
/// retained through completion. An error is not permission to recycle it. — TIMBERDOODLE
pub unsafe fn identify(
    protocol: *mut Protocol,
    nsid: u32,
    cns: u32,
    buffer: *mut u8,
) -> Result<(), Error> {
    if protocol.is_null()
        || buffer.is_null()
        || buffer as usize & (PAGE - 1) != 0
        || !((cns == 1 && nsid == 0) || (cns == 0 && nsid != 0 && nsid != u32::MAX))
    {
        return Err(Error::Input);
    }
    let mut command = Command {
        opcode: 6,
        flags: 4,
        nsid,
        cdw10: cns,
        ..Command::default()
    };
    let mut completion = Completion {
        words: [u32::MAX; 4],
    };
    let mut packet = Packet {
        timeout: TIMEOUT_100NS,
        buffer: buffer.cast(),
        length: PAGE as u32,
        metadata: ptr::null_mut(),
        metadata_length: 0,
        queue: 0,
        command: &mut command,
        completion: &mut completion,
    };
    let call = unsafe { (*protocol).pass_thru };
    let status = unsafe { call(protocol, nsid, &mut packet, ptr::null_mut()) };
    if status != 0 {
        return Err(Error::Firmware(status));
    }
    if packet.length != PAGE as u32 {
        return Err(Error::Short(packet.length));
    }
    if (completion.words[3] >> 17) & 0x7ff != 0 {
        return Err(Error::Completion(completion.words[3]));
    }
    Ok(())
}

fn ascii<const N: usize>(b: &[u8]) -> Result<[u8; N], Error> {
    let value: [u8; N] = b.try_into().map_err(|_| Error::Data)?;
    if !value.iter().all(|&v| (32..=126).contains(&v)) || value.iter().all(|&v| v == 32) {
        return Err(Error::Data);
    }
    Ok(value)
}
fn string(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap().trim_end_matches(' ')
}
fn le64(b: &[u8], n: usize) -> u64 {
    u64::from_le_bytes(b[n..n + 8].try_into().unwrap())
}
fn le32(b: &[u8], n: usize) -> u32 {
    u32::from_le_bytes(b[n..n + 4].try_into().unwrap())
}

pub struct Controller {
    pub vid: u16,
    pub ssvid: u16,
    serial: [u8; 20],
    model: [u8; 40],
    firmware: [u8; 8],
    pub version: u32,
    pub mdts: u8,
    pub namespaces: u32,
    pub vwc: bool,
}
impl Controller {
    pub fn model(&self) -> &str {
        string(&self.model)
    }
    pub fn serial(&self) -> &str {
        string(&self.serial)
    }
    pub fn firmware(&self) -> &str {
        string(&self.firmware)
    }
    pub fn parse(b: &[u8]) -> Result<Self, Error> {
        if b.len() != PAGE {
            return Err(Error::Data);
        }
        let vid = u16::from_le_bytes([b[0], b[1]]);
        if matches!(vid, 0 | 0xffff) || le32(b, 516) == 0 {
            return Err(Error::Data);
        }
        Ok(Self {
            vid,
            ssvid: u16::from_le_bytes([b[2], b[3]]),
            serial: ascii(&b[4..24])?,
            model: ascii(&b[24..64])?,
            firmware: ascii(&b[64..72])?,
            version: le32(b, 80),
            mdts: b[77],
            namespaces: le32(b, 516),
            vwc: b[525] & 1 != 0,
        })
    }
}
impl fmt::Display for Controller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,"NV4 UEFI CONTROLLER vid={:#x} ssvid={:#x} model=\"{}\" serial=\"{}\" firmware=\"{}\" version={:#x} mdts={} nn={} vwc={}",
            self.vid,self.ssvid,string(&self.model),string(&self.serial),string(&self.firmware),self.version,self.mdts,self.namespaces,self.vwc)
    }
}
pub struct Namespace {
    pub id: u32,
    pub size: u64,
    pub capacity: u64,
    pub used: u64,
    pub format: u8,
    pub lba: u64,
    pub bytes: u64,
    pub metadata: u16,
    pub protection: u8,
}
impl Namespace {
    pub fn parse(id: u32, b: &[u8]) -> Result<Self, Error> {
        if b.len() != PAGE || matches!(id, 0 | u32::MAX) || b[25] > 15 || b[26] & 0xe0 != 0 {
            return Err(Error::Data);
        }
        let format = b[26] & 15;
        if format > b[25] {
            return Err(Error::Data);
        }
        let index = 128 + usize::from(format) * 4;
        let exponent = b[index + 2];
        if !(9..=31).contains(&exponent) {
            return Err(Error::Data);
        }
        let lba = 1u64 << exponent;
        let size = le64(b, 0);
        let capacity = le64(b, 8);
        let used = le64(b, 16);
        if size == 0 || capacity == 0 || capacity > size || used > capacity {
            return Err(Error::Data);
        }
        Ok(Self {
            id,
            size,
            capacity,
            used,
            format,
            lba,
            bytes: size.checked_mul(lba).ok_or(Error::Data)?,
            metadata: u16::from_le_bytes([b[index], b[index + 1]]),
            protection: b[29] & 7,
        })
    }
}
impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,"NV4 UEFI NAMESPACE nsid={} nsze={} ncap={} nuse={} lbaf={} lba={} bytes={} metadata={} protection={}",
            self.id,self.size,self.capacity,self.used,self.format,self.lba,self.bytes,self.metadata,self.protection)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use core::mem::{offset_of, size_of};
    #[repr(align(4096))]
    struct Page([u8; PAGE]);
    unsafe extern "efiapi" fn next(_: *mut Protocol, _: *mut u32) -> usize {
        panic!("not requested")
    }
    unsafe extern "efiapi" fn pass(
        this: *mut Protocol,
        nsid: u32,
        p: *mut Packet,
        event: *mut c_void,
    ) -> usize {
        let p = unsafe { &mut *p };
        let cmd = unsafe { &*p.command };
        assert!(event.is_null());
        assert_eq!(p.timeout, 300_000_000);
        assert_eq!(p.queue, 0);
        assert!(p.metadata.is_null());
        assert_eq!(p.metadata_length, 0);
        assert_eq!(p.length, 4096);
        assert_eq!(cmd.opcode, 6);
        assert_eq!(cmd.flags, 4);
        assert_eq!(cmd.nsid, nsid);
        assert_eq!(cmd.reserved, [0; 3]);
        assert_eq!(
            [cmd.cdw2, cmd.cdw3, cmd.cdw11, cmd.cdw12, cmd.cdw13, cmd.cdw14, cmd.cdw15],
            [0; 7]
        );
        let mode = unsafe { &*(*this).mode };
        match mode.version {
            1 => return 18usize | (1usize << 63),
            2 => p.length = 512,
            _ => {}
        }
        unsafe {
            (*p.completion).words = [0, 0, 0, if mode.version == 3 { 1 << 17 } else { 0 }];
        }
        unsafe {
            ptr::write_bytes(p.buffer.cast::<u8>(), 0, PAGE);
        }
        0
    }
    fn protocol(mode: &Mode) -> Protocol {
        Protocol {
            mode,
            pass_thru: pass,
            next_namespace: next,
            build_device_path: ptr::null(),
            get_namespace: ptr::null(),
        }
    }
    #[test]
    fn abi_offsets_match_uefi_64_bit_and_edk2_header() {
        assert_eq!(size_of::<Mode>(), 12);
        assert_eq!(size_of::<Command>(), 44);
        assert_eq!(offset_of!(Command, flags), 4);
        assert_eq!(offset_of!(Command, nsid), 8);
        assert_eq!(offset_of!(Command, cdw10), 20);
        assert_eq!(size_of::<Completion>(), 16);
        assert_eq!(size_of::<Packet>(), 56);
        assert_eq!(offset_of!(Packet, buffer), 8);
        assert_eq!(offset_of!(Packet, length), 16);
        assert_eq!(offset_of!(Packet, metadata), 24);
        assert_eq!(offset_of!(Packet, metadata_length), 32);
        assert_eq!(offset_of!(Packet, queue), 36);
        assert_eq!(offset_of!(Packet, command), 40);
        assert_eq!(offset_of!(Packet, completion), 48);
        assert_eq!(size_of::<Protocol>(), 40);
        assert_eq!(offset_of!(Protocol, pass_thru), 8);
        assert_eq!(offset_of!(Protocol, next_namespace), 16);
    }
    #[test]
    fn board_and_alignment_scope_is_exact() {
        assert!(eligible("orange-pi-5-plus"));
        for b in [
            "",
            "qemu-virt-aarch64",
            "thinkpad-x13s-gen1",
            "generic-uefi-x86_64",
        ] {
            assert!(!eligible(b));
        }
        for align in [0, 1, 2, 64, 4096] {
            assert!(supported(&Mode {
                attributes: 1,
                io_align: align,
                version: 0x10300
            }));
        }
        for (attributes, io_align) in [(0, 1), (2, 4096), (1, 3), (1, 8192)] {
            assert!(!supported(&Mode {
                attributes,
                io_align,
                version: 0x10300
            }));
        }
    }
    #[test]
    fn blocking_identify_contract_checks_transport_length_and_completion() {
        let mut page = Page([0; PAGE]);
        for version in 0..4 {
            let mode = Mode {
                attributes: 1,
                io_align: 4096,
                version,
            };
            let mut p = protocol(&mode);
            let r = unsafe { identify(&mut p, 0, 1, page.0.as_mut_ptr()) };
            match version {
                0 => assert_eq!(r, Ok(())),
                1 => assert!(matches!(r, Err(Error::Firmware(_)))),
                2 => assert_eq!(r, Err(Error::Short(512))),
                _ => assert_eq!(r, Err(Error::Completion(1 << 17))),
            }
        }
        let mode = Mode {
            attributes: 1,
            io_align: 4096,
            version: 0,
        };
        let mut p = protocol(&mode);
        assert_eq!(
            unsafe { identify(&mut p, 7, 0, page.0.as_mut_ptr()) },
            Ok(())
        );
        for (ns, cns) in [(0, 0), (u32::MAX, 0), (1, 1), (0, 2)] {
            assert_eq!(
                unsafe { identify(&mut p, ns, cns, page.0.as_mut_ptr()) },
                Err(Error::Input)
            );
        }
        assert_eq!(
            unsafe { identify(&mut p, 0, 1, page.0.as_mut_ptr().wrapping_add(1)) },
            Err(Error::Input)
        );
    }
    #[test]
    fn reference_decoding_requires_real_identity_and_checked_geometry() {
        let mut b = [0; PAGE];
        assert!(Controller::parse(&b).is_err());
        b[0..2].copy_from_slice(&0x144du16.to_le_bytes());
        b[2..4].copy_from_slice(&0x144du16.to_le_bytes());
        b[4..72].fill(b' ');
        b[4..6].copy_from_slice(b"SN");
        b[24..26].copy_from_slice(b"MN");
        b[64..66].copy_from_slice(b"FR");
        b[516] = 1;
        b[77] = 9;
        let c = Controller::parse(&b).unwrap();
        assert_eq!(c.vid, 0x144d);
        assert_eq!(c.mdts, 9);
        b[64] = 0;
        assert!(Controller::parse(&b).is_err());
        b.fill(0);
        b[..8].copy_from_slice(&976773168u64.to_le_bytes());
        b[8..16].copy_from_slice(&976773168u64.to_le_bytes());
        b[130] = 9;
        let n = Namespace::parse(1, &b).unwrap();
        assert_eq!(n.bytes, 500107862016);
        b[26] = 1;
        assert!(Namespace::parse(1, &b).is_err());
        b[26] = 0;
        b[..8].fill(255);
        assert!(Namespace::parse(1, &b).is_err());
    }
}
