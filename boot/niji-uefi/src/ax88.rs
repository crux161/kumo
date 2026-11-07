//j810
//j832
//! Independent firmware-cache descriptor reference for the observed USB2 ax88 layout.
//! No shared drv-xhci parser. NET5a adds an explicit vendor-IN-only firmware reader.
//! — TIMBERDOODLE
use core::ffi::c_void;

/// UEFI2.11 section17.2.5/6. Only control-IN and cached getters are callable here;
/// bulk/interrupt/reset entries remain opaque. — TIMBERDOODLE
#[repr(C)]
pub struct Protocol {
    pub control: Option<
        extern "efiapi" fn(
            *mut Self,
            *mut DeviceRequest,
            u32,
            u32,
            *mut c_void,
            usize,
            *mut u32,
        ) -> usize,
    >,
    transfers: [*const c_void; 5],
    pub get_device: extern "efiapi" fn(*mut Self, *mut [u8; 18]) -> usize,
    pub get_config: extern "efiapi" fn(*mut Self, *mut [u8; 9]) -> usize,
    pub get_interface: extern "efiapi" fn(*mut Self, *mut [u8; 9]) -> usize,
    pub get_endpoint: extern "efiapi" fn(*mut Self, u8, *mut [u8; 7]) -> usize,
    unused_tail: [*const c_void; 3],
}

/// Wire-sized request; firmware consumes native little-endian fields on our targets.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceRequest {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}
impl DeviceRequest {
    pub fn bytes(self) -> [u8; 8] {
        let v = self.value.to_le_bytes();
        let i = self.index.to_le_bytes();
        let n = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            v[0],
            v[1],
            i[0],
            i[1],
            n[0],
            n[1],
        ]
    }
}

/// Independently transcribed from the supplied ASIX/Linux reference. Do not share
/// KUMO's request encoder: this is the second transport/value witness. — TIMBERDOODLE
pub const IDENTITY_READS: [(&str, u8, u16, u16, u16); 10] = [
    ("node_id", 1, 0x10, 6, 6),
    ("usb_link", 1, 2, 1, 1),
    ("silicon_status", 1, 3, 1, 1),
    ("phy_power_reset", 1, 0x26, 2, 2),
    ("clock", 1, 0x33, 1, 1),
    ("phy_page", 2, 3, 31, 2),
    ("phy_id1", 2, 3, 2, 2),
    ("phy_id2", 2, 3, 3, 2),
    ("phy_basic_status", 2, 3, 1, 2),
    ("phy_physical_status", 2, 3, 17, 2),
];

/// The protocol has no OUT actual-length parameter; success here attests firmware's
/// status only, not an independently measured USB byte count. — TIMBERDOODLE
/// # Safety
/// `usb` must be a live USB_IO interface and the buffer must outlive this call.
pub unsafe fn identity_read(
    usb: *mut Protocol,
    index: usize,
    data: &mut [u8; 6],
) -> Option<(DeviceRequest, usize, u32)> {
    let &(_, request, value, register, length) = IDENTITY_READS.get(index)?;
    let mut setup = DeviceRequest {
        request_type: 0xc0,
        request,
        value,
        index: register,
        length,
    };
    // SAFETY: caller supplies a live USB_IO interface; no pointer escapes.
    let control = unsafe { (*usb).control }?;
    let mut result = u32::MAX;
    let sent = setup;
    let status = control(
        usb,
        &mut setup,
        0,
        200,
        data.as_mut_ptr().cast(),
        length as usize,
        &mut result,
    );
    Some((sent, status, result))
}

pub fn matches(device: &[u8; 18]) -> bool {
    device[..2] == [18, 1]
        && device[8..10] == [0x95, 0x0b]
        && matches!(
            u16::from_le_bytes([device[10], device[11]]),
            0x1790 | 0x178a
        )
}

/// A total length of39 proves that the cached9+9+3x7 descriptors cover this
/// whole configuration. Do not fabricate unknown extra/SS descriptors.
/// Endpoint indexes preserve firmware's descriptor order. — TIMBERDOODLE
pub fn assemble(
    device: &[u8; 18],
    config: &[u8; 9],
    interface: &[u8; 9],
    endpoints: &[[u8; 7]; 3],
) -> Option<[u8; 39]> {
    if !matches(device)
        || device[17] != 1
        || device[7] != 64
        || u16::from_le_bytes([device[2], device[3]]) > 0x210
        || config[..2] != [9, 2]
        || config[2..5] != [39, 0, 1]
        || config[5] == 0
        || interface[..2] != [9, 4]
        || interface[2..8] != [0, 0, 3, 0xff, 0xff, 0]
        || endpoints.iter().any(|e| e[..2] != [7, 5])
    {
        return None;
    }
    let mut bytes = [0; 39];
    bytes[..9].copy_from_slice(config);
    bytes[9..18].copy_from_slice(interface);
    for (i, e) in endpoints.iter().enumerate() {
        bytes[18 + i * 7..25 + i * 7].copy_from_slice(e);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_layout_matches_uefi_offsets() {
        use core::mem::{offset_of, size_of};
        let p = size_of::<usize>();
        assert_eq!(offset_of!(Protocol, control), 0);
        assert_eq!(size_of::<DeviceRequest>(), 8);
        assert_eq!(offset_of!(DeviceRequest, value), 2);
        assert_eq!(offset_of!(DeviceRequest, length), 6);
        assert_eq!(offset_of!(Protocol, get_device), 6 * p);
        assert_eq!(offset_of!(Protocol, get_config), 7 * p);
        assert_eq!(offset_of!(Protocol, get_interface), 8 * p);
        assert_eq!(offset_of!(Protocol, get_endpoint), 9 * p);
        assert_eq!(size_of::<Protocol>(), 13 * p);
    }
    #[test]
    fn firmware_control_abi_uses_in_direction_bounded_timeout_and_input_length() {
        extern "efiapi" fn control(
            _: *mut Protocol,
            request: *mut DeviceRequest,
            direction: u32,
            timeout: u32,
            data: *mut c_void,
            length: usize,
            result: *mut u32,
        ) -> usize {
            assert_eq!((direction, timeout), (0, 200));
            let request = unsafe { *request };
            assert_eq!(request.request_type, 0xc0);
            assert!(matches!(request.request, 1 | 2));
            assert_eq!(length, request.length as usize);
            assert!((1..=6).contains(&length));
            for i in 0..length {
                unsafe { data.cast::<u8>().add(i).write(i as u8) }
            }
            unsafe { *result = 0 };
            0
        }
        extern "efiapi" fn device(_: *mut Protocol, _: *mut [u8; 18]) -> usize {
            0
        }
        extern "efiapi" fn desc(_: *mut Protocol, _: *mut [u8; 9]) -> usize {
            0
        }
        extern "efiapi" fn endpoint(_: *mut Protocol, _: u8, _: *mut [u8; 7]) -> usize {
            0
        }
        let mut usb = Protocol {
            control: Some(control),
            transfers: [core::ptr::null(); 5],
            get_device: device,
            get_config: desc,
            get_interface: desc,
            get_endpoint: endpoint,
            unused_tail: [core::ptr::null(); 3],
        };
        for i in 0..IDENTITY_READS.len() {
            let mut data = [0xa5; 6];
            let (setup, status, result) = unsafe { identity_read(&mut usb, i, &mut data) }.unwrap();
            assert_eq!((status, result), (0, 0));
            assert_eq!(setup.bytes()[0], 0xc0);
            for (at, byte) in data.iter().enumerate() {
                assert_eq!(
                    *byte,
                    if at < setup.length as usize {
                        at as u8
                    } else {
                        0xa5
                    }
                );
            }
        }
        assert!(unsafe { identity_read(&mut usb, 10, &mut [0; 6]) }.is_none());
        usb.control = None;
        assert!(unsafe { identity_read(&mut usb, 0, &mut [0; 6]) }.is_none());
    }
    fn fixture() -> ([u8; 18], [u8; 9], [u8; 9], [[u8; 7]; 3]) {
        (
            [
                18, 1, 0x10, 2, 0xff, 0xff, 0, 64, 0x95, 0x0b, 0x90, 0x17, 0, 1, 1, 2, 3, 1,
            ],
            [9, 2, 39, 0, 1, 1, 0, 0xe0, 2],
            [9, 4, 0, 0, 3, 0xff, 0xff, 0, 4],
            [
                [7, 5, 0x81, 3, 8, 0, 11],
                [7, 5, 0x82, 2, 0, 2, 0],
                [7, 5, 3, 2, 0, 2, 0],
            ],
        )
    }
    #[test]
    fn cached_usb2_layout_reconstructs_without_shared_decoder() {
        let (d, c, i, e) = fixture();
        let bytes = assemble(&d, &c, &i, &e).unwrap();
        assert_eq!(&bytes[..9], &c);
        assert_eq!(&bytes[9..18], &i);
        assert_eq!(&bytes[32..], &e[2]);
        let mut d = d;
        d[10] = 0x8a;
        assert!(assemble(&d, &c, &i, &e).is_some());
    }
    #[test]
    fn other_ids_ss_extra_descriptors_and_other_interfaces_are_unavailable() {
        let (d, c, i, e) = fixture();
        for (at, val) in [(0, 0), (8, 0), (10, 0x91), (17, 2), (3, 3), (7, 9)] {
            let mut x = d;
            x[at] = val;
            assert!(assemble(&x, &c, &i, &e).is_none());
        }
        for (at, val) in [(2, 57), (4, 2), (5, 0)] {
            let mut x = c;
            x[at] = val;
            assert!(assemble(&d, &x, &i, &e).is_none());
        }
        for (at, val) in [(2, 1), (3, 1), (4, 2), (5, 2)] {
            let mut x = i;
            x[at] = val;
            assert!(assemble(&d, &c, &x, &e).is_none());
        }
        let mut e = e;
        e[1][0] = 6;
        assert!(assemble(&d, &c, &i, &e).is_none());
    }
}
