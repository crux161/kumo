//j804
//! Emit only the sparse partition image for independent, read-only fsck.
use niji_uefi::nvme_prepare::{sector, DATA, LENGTH, START};
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
};
fn main() {
    let path = std::env::args().nth(1).expect("output path");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    f.set_len(LENGTH * 512).unwrap();
    for lba in 0..=DATA + 1 {
        f.seek(SeekFrom::Start(lba * 512)).unwrap();
        f.write_all(&sector(START + lba).unwrap()).unwrap();
    }
    f.sync_all().unwrap();
}
