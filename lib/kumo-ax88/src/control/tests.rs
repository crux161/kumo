//j826
use super::*;

#[test]
fn golden_mac_read_and_volatile_write_setup_payloads() {
    let node = Request::mac_read(MacRegister::NodeId);
    assert_eq!(node.setup(), [0xc0, 1, 0x10, 0, 6, 0, 6, 0]);
    assert_eq!(node.direction(), Direction::In);
    assert!(node.outbound().is_empty());
    let rx = Request::mac_write(MacRegister::RxControl, &0x0280u16.to_le_bytes()).unwrap();
    assert_eq!(rx.setup(), [0x40, 1, 0x0b, 0, 2, 0, 2, 0]);
    assert_eq!(rx.outbound(), [0x80, 2]);
    let queue = Request::mac_write(MacRegister::RxBulkQueue, &[7, 0x4f, 0, 0x12, 0xff]).unwrap();
    assert_eq!(queue.setup(), [0x40, 1, 0x2e, 0, 5, 0, 5, 0]);
    assert_eq!(queue.outbound(), [7, 0x4f, 0, 0x12, 0xff]);
}

#[test]
fn all_named_register_widths_are_exact_and_readonly_registers_cannot_write() {
    for (reg, address, width, writable) in [
        (MacRegister::UsbLink, 0x02, 1, false),
        (MacRegister::SiliconStatus, 0x03, 1, false),
        (MacRegister::RxControl, 0x0b, 2, true),
        (MacRegister::NodeId, 0x10, 6, false),
        (MacRegister::Medium, 0x22, 2, true),
        (MacRegister::PhyPowerReset, 0x26, 2, true),
        (MacRegister::RxBulkQueue, 0x2e, 5, true),
        (MacRegister::RxChecksum, 0x34, 1, true),
        (MacRegister::TxChecksum, 0x35, 1, true),
        (MacRegister::Clock, 0x33, 1, true),
    ] {
        let r = Request::mac_read(reg);
        assert_eq!(r.transfer_len(), width);
        assert_eq!(reg.width(), width);
        assert_eq!(
            r.setup(),
            [0xc0, 1, address, 0, width as u8, 0, width as u8, 0]
        );
        for actual in 0..=8 {
            assert_eq!(r.validate_transferred(actual).is_ok(), actual == width);
        }
        assert_eq!(
            r.validate_transferred(usize::MAX),
            Err(Error::CompletionLength)
        );
        for len in 0..=8 {
            let bytes = [0u8; 8];
            let result = Request::mac_write(reg, &bytes[..len]);
            if !writable {
                assert_eq!(result, Err(Error::ReadOnly));
            } else {
                assert_eq!(result.is_ok(), len == width);
            }
        }
    }
}

#[test]
fn phy_wire_is_little_endian_and_never_selects_a_page_implicitly() {
    assert_eq!(
        Request::phy_read(0x11).unwrap().setup(),
        [0xc0, 2, 3, 0, 0x11, 0, 2, 0]
    );
    let page = Request::phy_write(31, 0x1234).unwrap();
    assert_eq!(page.setup(), [0x40, 2, 3, 0, 31, 0, 2, 0]);
    assert_eq!(page.outbound(), [0x34, 0x12]);
    for reg in 0..=u8::MAX {
        assert_eq!(Request::phy_read(reg).is_ok(), reg <= 31);
        assert_eq!(Request::phy_write(reg, 0xffff).is_ok(), reg <= 31);
    }
    for actual in [0, 1, 3, usize::MAX] {
        assert_eq!(
            page.validate_transferred(actual),
            Err(Error::CompletionLength)
        );
    }
    assert_eq!(page.validate_transferred(2), Ok(()));
}

#[test]
fn mac_address_requires_exact_nonzero_unicast_value() {
    for bytes in [&[][..], &[0; 5], &[0; 7]] {
        assert_eq!(decode_mac(bytes), Err(Error::Length));
    }
    for bytes in [
        [0; 6],
        [0xff; 6],
        [1, 0, 0, 0, 0, 0],
        [0x33, 0x33, 0, 0, 0, 1],
    ] {
        assert_eq!(decode_mac(&bytes), Err(Error::InvalidMac));
    }
    for bytes in [[2, 0, 0, 0, 0, 1], [0, 0x0e, 0xc6, 0x11, 0x22, 0x33]] {
        assert_eq!(decode_mac(&bytes), Ok(bytes));
    }
}
