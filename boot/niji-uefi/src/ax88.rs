//j810
//! Independent firmware-cache descriptor reference for the observed USB2 ax88 layout.
//! No drv-xhci parser, USB request, register access or DMA allocation. — TIMBERDOODLE
use core::ffi::c_void;

/// UEFI UsbIo.h: six transfer methods, four cached descriptor getters, then
/// string/language/reset. Only cached getters are callable here. — TIMBERDOODLE
#[repr(C)]
pub struct Protocol {
    transfers: [*const c_void; 6],
    pub get_device: extern "efiapi" fn(*mut Self, *mut [u8; 18]) -> usize,
    pub get_config: extern "efiapi" fn(*mut Self, *mut [u8; 9]) -> usize,
    pub get_interface: extern "efiapi" fn(*mut Self, *mut [u8; 9]) -> usize,
    pub get_endpoint: extern "efiapi" fn(*mut Self, u8, *mut [u8; 7]) -> usize,
    unused_tail: [*const c_void; 3],
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
        assert_eq!(offset_of!(Protocol, get_device), 6 * p);
        assert_eq!(offset_of!(Protocol, get_config), 7 * p);
        assert_eq!(offset_of!(Protocol, get_interface), 8 * p);
        assert_eq!(offset_of!(Protocol, get_endpoint), 9 * p);
        assert_eq!(size_of::<Protocol>(), 13 * p);
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
