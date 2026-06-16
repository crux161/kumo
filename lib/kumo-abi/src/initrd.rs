pub const INITRD_MAGIC: [u8; 8] = *b"KUMORD01";
pub const INITRD_HEADER_LEN: usize = 16;
pub const INITRD_ENTRY_LEN: usize = 80;
pub const INITRD_PATH_MAX: usize = 64;
pub const INITRD_VERSION: u32 = 1;
pub const SORA_INIT_PATH: &str = "bin/sora";
pub const SVC_HEALTH_PATH: &str = "bin/svc-health";
pub const FAT32_IMG_PATH: &str = "bin/fat32.img";
pub const PERSONA_LINUX_HELLO_PATH: &str = "bin/persona-linux-hello";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitrdError {
    TooSmall,
    BadMagic,
    BadVersion,
    TruncatedTable,
    BadPath,
    BadRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitrdFile<'a> {
    pub path: &'a str,
    pub offset: u64,
    pub bytes: &'a [u8],
}

pub fn find_file<'a>(initrd: &'a [u8], path: &str) -> Result<Option<InitrdFile<'a>>, InitrdError> {
    if initrd.len() < INITRD_HEADER_LEN {
        return Err(InitrdError::TooSmall);
    }
    if initrd[..8] != INITRD_MAGIC {
        return Err(InitrdError::BadMagic);
    }

    let version = read_u32(initrd, 8)?;
    if version != INITRD_VERSION {
        return Err(InitrdError::BadVersion);
    }

    let entry_count = read_u32(initrd, 12)? as usize;
    let table_bytes = entry_count
        .checked_mul(INITRD_ENTRY_LEN)
        .ok_or(InitrdError::BadRange)?;
    let table_end = INITRD_HEADER_LEN
        .checked_add(table_bytes)
        .ok_or(InitrdError::BadRange)?;
    if table_end > initrd.len() {
        return Err(InitrdError::TruncatedTable);
    }

    for index in 0..entry_count {
        let base = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN;
        let path_bytes = &initrd[base..base + INITRD_PATH_MAX];
        let path_len = path_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(INITRD_PATH_MAX);
        let entry_path =
            core::str::from_utf8(&path_bytes[..path_len]).map_err(|_| InitrdError::BadPath)?;
        let offset = read_u64(initrd, base + INITRD_PATH_MAX)?;
        let len = read_u64(initrd, base + INITRD_PATH_MAX + 8)?;

        let start = usize::try_from(offset).map_err(|_| InitrdError::BadRange)?;
        let len = usize::try_from(len).map_err(|_| InitrdError::BadRange)?;
        let end = start.checked_add(len).ok_or(InitrdError::BadRange)?;
        if start < table_end || end > initrd.len() {
            return Err(InitrdError::BadRange);
        }

        if entry_path == path {
            return Ok(Some(InitrdFile {
                path: entry_path,
                offset,
                bytes: &initrd[start..end],
            }));
        }
    }

    Ok(None)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, InitrdError> {
    let end = offset.checked_add(4).ok_or(InitrdError::BadRange)?;
    if end > bytes.len() {
        return Err(InitrdError::TooSmall);
    }
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, InitrdError> {
    let end = offset.checked_add(8).ok_or(InitrdError::BadRange)?;
    if end > bytes.len() {
        return Err(InitrdError::TooSmall);
    }
    Ok(u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    fn one_file(path: &str, bytes: &[u8]) -> std::vec::Vec<u8> {
        let mut initrd = std::vec![0; INITRD_HEADER_LEN + INITRD_ENTRY_LEN];
        initrd[..8].copy_from_slice(&INITRD_MAGIC);
        initrd[8..12].copy_from_slice(&INITRD_VERSION.to_le_bytes());
        initrd[12..16].copy_from_slice(&1u32.to_le_bytes());

        let path_bytes = path.as_bytes();
        initrd[INITRD_HEADER_LEN..INITRD_HEADER_LEN + path_bytes.len()].copy_from_slice(path_bytes);
        let offset = initrd.len() as u64;
        initrd[INITRD_HEADER_LEN + INITRD_PATH_MAX..INITRD_HEADER_LEN + INITRD_PATH_MAX + 8]
            .copy_from_slice(&offset.to_le_bytes());
        initrd[INITRD_HEADER_LEN + INITRD_PATH_MAX + 8..INITRD_HEADER_LEN + INITRD_PATH_MAX + 16]
            .copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        initrd.extend_from_slice(bytes);
        initrd
    }

    #[test]
    fn finds_named_file() {
        let initrd = one_file(SORA_INIT_PATH, b"elf-ish");
        let file = find_file(&initrd, SORA_INIT_PATH).unwrap().unwrap();
        assert_eq!(file.path, SORA_INIT_PATH);
        assert_eq!(file.bytes, b"elf-ish");
    }

    #[test]
    fn rejects_bad_ranges() {
        let mut initrd = one_file(SORA_INIT_PATH, b"elf-ish");
        let bad_offset = 8u64;
        initrd[INITRD_HEADER_LEN + INITRD_PATH_MAX..INITRD_HEADER_LEN + INITRD_PATH_MAX + 8]
            .copy_from_slice(&bad_offset.to_le_bytes());
        assert_eq!(
            find_file(&initrd, SORA_INIT_PATH),
            Err(InitrdError::BadRange)
        );
    }
}
