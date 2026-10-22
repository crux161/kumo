#![no_std]
//j549

use drv_blk::{
    Request, BLOCK_SIZE, CMD_FLUSH, CMD_READ, CMD_WRITE, REQUEST_LEN, STATUS_BAD_LBA,
    STATUS_BAD_LEN, STATUS_READ_ONLY,
};

/// One bounded operation on the kept USB volume, with a device-relative LBA.
#[derive(Debug, PartialEq, Eq)]
pub enum StorageRequest<'a> {
    Read(u64),
    Write(u64, &'a [u8]),
    Flush,
}

/// Validate the complete frame before any USB transaction or write touches the stick.
/// The service implements one 512-byte block per request. — KESTREL
pub fn storage_request(
    frame: &[u8],
    first_lba: u64,
    block_count: u64,
    read_only: bool,
) -> Result<StorageRequest<'_>, u8> {
    let request = Request::decode(frame).ok_or(STATUS_BAD_LEN)?;
    match request.cmd {
        CMD_FLUSH if frame.len() == REQUEST_LEN => return Ok(StorageRequest::Flush),
        CMD_READ if frame.len() == REQUEST_LEN => {}
        CMD_WRITE if frame.len() == REQUEST_LEN + BLOCK_SIZE as usize => {}
        _ => return Err(STATUS_BAD_LEN),
    }
    if request.count != 1 {
        return Err(STATUS_BAD_LEN);
    }
    if request.lba >= block_count {
        return Err(STATUS_BAD_LBA);
    }
    let lba = first_lba.checked_add(request.lba).ok_or(STATUS_BAD_LBA)?;
    if request.cmd == CMD_READ {
        Ok(StorageRequest::Read(lba))
    } else if read_only {
        Err(STATUS_READ_ONLY)
    } else {
        Ok(StorageRequest::Write(lba, &frame[REQUEST_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_exactly_one_sector_inside_the_partition() {
        let mut frame = [0xAB; REQUEST_LEN + BLOCK_SIZE as usize];
        frame[..REQUEST_LEN].copy_from_slice(&Request::write(3, 1).encode());
        assert_eq!(
            storage_request(&frame, 2048, 4, false),
            Ok(StorageRequest::Write(2051, &frame[REQUEST_LEN..]))
        );
        assert_eq!(
            storage_request(&frame, 2048, 4, true),
            Err(STATUS_READ_ONLY)
        );
        assert_eq!(storage_request(&frame, 2048, 3, false), Err(STATUS_BAD_LBA));
        assert_eq!(
            storage_request(&frame, u64::MAX, 4, false),
            Err(STATUS_BAD_LBA)
        );
        assert_eq!(
            storage_request(&frame[..frame.len() - 1], 2048, 4, false),
            Err(STATUS_BAD_LEN)
        );
        frame[..REQUEST_LEN].copy_from_slice(&Request::write(0, 2).encode());
        assert_eq!(storage_request(&frame, 2048, 4, false), Err(STATUS_BAD_LEN));
        frame[..REQUEST_LEN].copy_from_slice(&Request::write(0, 0).encode());
        assert_eq!(storage_request(&frame, 2048, 4, false), Err(STATUS_BAD_LEN));
    }

    #[test]
    fn reads_and_flushes_keep_the_existing_wire_contract() {
        assert_eq!(
            storage_request(&Request::read(0, 1).encode(), 2048, 4, false),
            Ok(StorageRequest::Read(2048))
        );
        assert_eq!(
            storage_request(&Request::flush().encode(), 2048, 4, true),
            Ok(StorageRequest::Flush)
        );
        let mut extra = [0; REQUEST_LEN + 1];
        extra[..REQUEST_LEN].copy_from_slice(&Request::read(0, 1).encode());
        assert_eq!(storage_request(&extra, 2048, 4, false), Err(STATUS_BAD_LEN));
    }
}
