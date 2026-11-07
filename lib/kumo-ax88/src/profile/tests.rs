//j826
extern crate std;
use super::*;
use std::vec::Vec;

// Synthetic descriptors use the NET0 observed VID/PID/revision/interface/endpoint
// fields; the SuperSpeed/178A cases are software fixtures, not metal evidence.
fn descriptors(pid: u16, speed: Speed) -> ([u8; 18], Vec<u8>) {
    let ss = speed == Speed::Super;
    let d = [
        18,
        1,
        if ss { 0 } else { 0x10 },
        if ss { 3 } else { 2 },
        0,
        0,
        0,
        if ss { 9 } else { 64 },
        0x95,
        0x0b,
        pid as u8,
        (pid >> 8) as u8,
        0,
        1,
        1,
        2,
        3,
        1,
    ];
    let bulk = match speed {
        Speed::Full => 64u16,
        Speed::High => 512,
        Speed::Super => 1024,
    };
    let mut c = std::vec![9, 2, 0, 0, 1, 1, 0, 0x80, 125, 9, 4, 0, 0, 3, 0xff, 0xff, 0, 0];
    // Deliberately put OUT first: selection is by direction/type, not array position.
    for (address, kind, mps, interval) in [(3, 2, bulk, 0), (0x81, 3, 8, 11), (0x82, 2, bulk, 0)] {
        c.extend_from_slice(&[7, 5, address, kind, mps as u8, (mps >> 8) as u8, interval]);
        if ss {
            c.extend_from_slice(&[
                6,
                0x30,
                if kind == 2 { 3 } else { 0 },
                0,
                if kind == 3 { 8 } else { 0 },
                0,
            ]);
        }
    }
    let n = (c.len() as u16).to_le_bytes();
    c[2..4].copy_from_slice(&n);
    (d, c)
}

#[test]
fn both_families_and_supported_speed_profiles_select_endpoint_roles() {
    for pid in [0x1790, 0x178a] {
        for speed in [Speed::Full, Speed::High, Speed::Super] {
            let (d, c) = descriptors(pid, speed);
            if pid == 0x178a && speed == Speed::Super {
                assert_eq!(Profile::from_descriptors(&d, &c, speed), Err(Error::Speed));
                continue;
            }
            let p = Profile::from_descriptors(&d, &c, speed).unwrap();
            assert_eq!(p.speed(), speed);
            assert_eq!(p.inventory().device_revision, 0x100);
            assert_eq!(p.rx().address, 0x82);
            assert_eq!(p.tx().address, 3);
            assert_eq!(p.status().address, 0x81);
            assert_eq!(
                p.rx().max_packet_size,
                match speed {
                    Speed::Full => 64,
                    Speed::High => 512,
                    Speed::Super => 1024,
                }
            );
        }
    }
}

#[test]
fn unknown_ids_revisions_speeds_and_transport_layouts_stay_unbound() {
    let (device, config) = descriptors(0x1790, Speed::High);
    for pid in [0, 0x1791, 0x8153] {
        let mut d = device;
        d[10..12].copy_from_slice(&(pid as u16).to_le_bytes());
        assert_eq!(
            Profile::from_descriptors(&d, &config, Speed::High),
            Err(Error::Unmatched)
        );
    }
    for revision in [0u16, 1, 0x0101, 0x0200, 0xffff] {
        let mut d = device;
        d[12..14].copy_from_slice(&revision.to_le_bytes());
        assert_eq!(
            Profile::from_descriptors(&d, &config, Speed::High),
            Err(Error::Revision)
        );
    }
    assert!(Profile::from_descriptors(&device, &config, Speed::Full).is_err());
    assert!(Profile::from_descriptors(&device, &config, Speed::Super).is_err());
    for (at, value) in [(7, 9), (2, 0xff), (3, 0xff)] {
        let mut d = device;
        d[at] = value;
        assert!(Profile::from_descriptors(&d, &config, Speed::High).is_err());
    }
    for (at, value) in [(21, 0x82), (22, 1), (23, 0), (24, 1), (29, 0), (31, 0)] {
        let mut c = config.clone();
        c[at] = value;
        assert!(
            Profile::from_descriptors(&device, &c, Speed::High).is_err(),
            "{at}"
        );
    }
}

#[test]
fn streams_and_bad_status_companions_refuse() {
    let (d, c) = descriptors(0x1790, Speed::Super);
    for (at, value) in [(28, 1), (39, 1), (40, 1), (42, 0), (54, 1)] {
        let mut b = c.clone();
        b[at] = value;
        assert!(
            Profile::from_descriptors(&d, &b, Speed::Super).is_err(),
            "{at}"
        );
    }
}

#[test]
fn every_truncation_and_byte_corruption_of_matched_descriptors_is_safe() {
    for speed in [Speed::High, Speed::Super] {
        let (d, c) = descriptors(0x1790, speed);
        for n in 0..d.len() {
            assert!(Profile::from_descriptors(&d[..n], &c, speed).is_err());
        }
        for n in 0..c.len() {
            assert!(Profile::from_descriptors(&d, &c[..n], speed).is_err());
        }
        for at in 0..c.len() {
            for value in 0..=255 {
                let mut b = c.clone();
                b[at] = value;
                let _ = Profile::from_descriptors(&d, &b, speed);
            }
        }
    }
}
