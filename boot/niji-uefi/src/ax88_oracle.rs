//j810
//! Default-off cached USB_IO reference; never calls transfer/reset methods.
use crate::{guid, print_line, EfiBootServices, EfiHandle, EfiSimpleTextOutputProtocol};
use core::{ffi::c_void, fmt, ptr};
use niji_uefi::ax88::{self, Protocol};

struct Hex<'a>(&'a [u8]);
impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Read only firmware's cached descriptors. Other layouts are unavailable, not
/// reconstructed by guessing. The only pool is firmware's handle list. — TIMBERDOODLE
pub unsafe fn run(bs: *mut EfiBootServices, con: *mut EfiSimpleTextOutputProtocol) {
    let id = guid(
        0x2b2f68d6,
        0x0cd2,
        0x44cf,
        [0x8e, 0x8b, 0xbb, 0xa2, 0x0b, 0x1b, 0x5b, 0x75],
    );
    let mut count = 0;
    let mut handles: *mut EfiHandle = ptr::null_mut();
    let status =
        unsafe { ((*bs).locate_handle_buffer)(2, &id, ptr::null_mut(), &mut count, &mut handles) };
    if status != 0 || handles.is_null() {
        print_line(
            con,
            format_args!("NET0 UEFI ax88 unavailable: no USB_IO handles\r\n"),
        );
        return;
    }
    let mut matches = 0;
    let mut references = 0;
    if count <= 256 {
        for index in 0..count {
            let handle = unsafe { *handles.add(index) };
            let mut interface: *mut c_void = ptr::null_mut();
            if unsafe { ((*bs).handle_protocol)(handle, &id, &mut interface) } != 0
                || interface.is_null()
            {
                continue;
            }
            let usb = interface.cast::<Protocol>();
            let mut device = [0; 18];
            if unsafe { ((*usb).get_device)(usb, &mut device) } != 0 || !ax88::matches(&device) {
                continue;
            }
            matches += 1;
            let (mut config, mut iface, mut eps) = ([0; 9], [0; 9], [[0; 7]; 3]);
            if unsafe { ((*usb).get_config)(usb, &mut config) } != 0
                || unsafe { ((*usb).get_interface)(usb, &mut iface) } != 0
            {
                continue;
            }
            let mut complete = true;
            for (i, ep) in eps.iter_mut().enumerate() {
                if unsafe { ((*usb).get_endpoint)(usb, i as u8, ep) } != 0 {
                    complete = false;
                    break;
                }
            }
            if !complete {
                continue;
            }
            let Some(config) = ax88::assemble(&device, &config, &iface, &eps) else {
                continue;
            };
            references += 1;
            print_line(
                con,
                format_args!(
                    "NET0 UEFI ax88 instance={} DEVICE {}\r\n",
                    index,
                    Hex(&device)
                ),
            );
            for (i, chunk) in config.chunks(16).enumerate() {
                print_line(
                    con,
                    format_args!(
                        "NET0 UEFI ax88 instance={} CONFIG offset={} {}\r\n",
                        index,
                        i * 16,
                        Hex(chunk)
                    ),
                );
            }
        }
    }
    let _ = unsafe { ((*bs).free_pool)(handles.cast()) };
    print_line(con,format_args!("NET0 UEFI ax88 cached reference: matches={} complete={} unique={} no transfer/reset call\r\n",matches,references,matches==1&&references==1));
}
