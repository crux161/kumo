pub const INITRD_MAGIC: [u8; 8] = *b"KUMORD01";
pub const INITRD_HEADER_LEN: usize = 16;
pub const INITRD_ENTRY_LEN: usize = 80;
pub const INITRD_PATH_MAX: usize = 64;
pub const INITRD_VERSION: u32 = 1;
pub const SORA_INIT_PATH: &str = "bin/sora";
pub const SVC_HEALTH_PATH: &str = "bin/svc-health";
pub const TTYD_PATH: &str = "bin/ttyd";
pub const DRV_SERIAL_PATH: &str = "bin/drv-serial";
pub const DRV_FB_PATH: &str = "bin/drv-fb";
pub const DRV_BLK_PATH: &str = "bin/drv-blk";
pub const FAT32_IMG_PATH: &str = "bin/fat32.img";
pub const PERSONA_LINUX_HELLO_PATH: &str = "bin/persona-linux-hello";
/// A from-scratch *native* KUMO userland program (DebugWrite + ProcessExit) — the
/// template a program author copies, and the exec-vertical proof binary.
pub const HELLO_PATH: &str = "bin/hello";
/// `ls` as a real program: receives a read-only initrd VMO handle in `x0` and prints
/// the entry list. The first program proven to *use* a passed capability (vs. the
/// capability-less `hello`); launched by the shell's `ls` builtin, which narrows the
/// initrd to read-only before granting it.
pub const LS_PATH: &str = "bin/ls";
/// `args` echoes the arguments it was launched with — the proof program for argv
/// passing: it receives a read-only argv VMO handle in `x1` and walks it with
/// [`crate::unpack_argv`].
pub const ARGS_PATH: &str = "bin/args";
/// `cat` receives a read-only initrd VMO in `x0` and argv in `x1`, then streams
/// the named initrd entry to standard debug output.
pub const CAT_PATH: &str = "bin/cat";
/// The input-less boot script Sora runs from the initrd: one `bin/<name>` per
/// line, `#` comments and blanks ignored. The X13s autoexec stopgap — programs
/// run and paint to the framebuffer with no keyboard. Parsed by `kumoza`.
/// Placeholder Lua REPL — prints a status message and exits. Piccolo is deferred
/// until vendored for offline builds (DEFERRED/003).
pub const LUA_REPL_PATH: &str = "bin/lua-repl";
pub const AUTOEXEC_PATH: &str = "etc/autoexec";

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

/// Metadata for one initrd entry, parsed from the header + entry table without
/// requiring the file payload to be present in the input buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitrdEntry<'a> {
    pub path: &'a str,
    pub offset: u64,
    pub len: u64,
}

/// Find a named entry using only the initrd header + complete entry table.
///
/// This is the streaming counterpart to [`find_file`]: callers such as `cat`
/// first read the small table, then read the payload directly from `offset` in
/// bounded chunks instead of mapping or buffering the whole initrd.
pub fn find_entry<'a>(table: &'a [u8], path: &str) -> Result<Option<InitrdEntry<'a>>, InitrdError> {
    let table_end = entry_table_bytes(table)?;
    if table_end > table.len() {
        return Err(InitrdError::TruncatedTable);
    }

    let entry_count = read_u32(table, 12)? as usize;
    for index in 0..entry_count {
        let base = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN;
        let path_bytes = &table[base..base + INITRD_PATH_MAX];
        let path_len = path_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(INITRD_PATH_MAX);
        let entry_path =
            core::str::from_utf8(&path_bytes[..path_len]).map_err(|_| InitrdError::BadPath)?;
        let offset = read_u64(table, base + INITRD_PATH_MAX)?;
        let len = read_u64(table, base + INITRD_PATH_MAX + 8)?;
        let _end = offset.checked_add(len).ok_or(InitrdError::BadRange)?;
        if offset < table_end as u64 {
            return Err(InitrdError::BadRange);
        }

        if entry_path == path {
            return Ok(Some(InitrdEntry {
                path: entry_path,
                offset,
                len,
            }));
        }
    }
    Ok(None)
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

/// Iterate the entry paths in an initrd, reading only the header + entry table from
/// `buf` — the file *data* need not be present, so a listing (the shell's `ls`) can
/// `vmo_read` just the first few KiB of the initrd VMO instead of the whole image.
///
/// Yields nothing if the magic or version is wrong. An entry whose path field or
/// non-UTF-8 path runs past `buf` is skipped, so a too-small buffer truncates the list
/// rather than faulting. The yielded `&str`s borrow from `buf`.
pub fn entry_paths(buf: &[u8]) -> impl Iterator<Item = &str> {
    entries(buf).map(|(path, _size)| path)
}

/// Like [`entry_paths`], but also yields each entry's **payload size in bytes** (the
/// `len` field of the entry, independent of the file data being present in `buf`). The
/// shell's `ls` uses this for an `ls -l`-style listing. Same magic/version guard and
/// past-`buf` truncation behavior as [`entry_paths`].
pub fn entries(buf: &[u8]) -> impl Iterator<Item = (&str, u64)> {
    let count = if buf.len() >= INITRD_HEADER_LEN
        && buf[..8] == INITRD_MAGIC
        && read_u32(buf, 8) == Ok(INITRD_VERSION)
    {
        read_u32(buf, 12).unwrap_or(0) as usize
    } else {
        0
    };
    (0..count).filter_map(move |index| {
        let base = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN;
        let path_field = buf.get(base..base + INITRD_PATH_MAX)?;
        let path_len = path_field
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(INITRD_PATH_MAX);
        let path = core::str::from_utf8(&path_field[..path_len]).ok()?;
        // Entry layout: path[0..64] · offset[64..72] · len[72..80] (LE).
        let size = read_u64(buf, base + INITRD_PATH_MAX + 8).ok()?;
        Some((path, size))
    })
}

/// Return the number of bytes needed to hold the initrd header plus its whole
/// entry table. File payload bytes are not included.
pub fn entry_table_bytes(buf: &[u8]) -> Result<usize, InitrdError> {
    if buf.len() < INITRD_HEADER_LEN {
        return Err(InitrdError::TooSmall);
    }
    if buf[..8] != INITRD_MAGIC {
        return Err(InitrdError::BadMagic);
    }

    let version = read_u32(buf, 8)?;
    if version != INITRD_VERSION {
        return Err(InitrdError::BadVersion);
    }

    let entry_count = read_u32(buf, 12)? as usize;
    let table_bytes = entry_count
        .checked_mul(INITRD_ENTRY_LEN)
        .ok_or(InitrdError::BadRange)?;
    INITRD_HEADER_LEN
        .checked_add(table_bytes)
        .ok_or(InitrdError::BadRange)
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

    fn table_of(paths: &[&str]) -> std::vec::Vec<u8> {
        let mut initrd = std::vec![0; INITRD_HEADER_LEN + paths.len() * INITRD_ENTRY_LEN];
        initrd[..8].copy_from_slice(&INITRD_MAGIC);
        initrd[8..12].copy_from_slice(&INITRD_VERSION.to_le_bytes());
        initrd[12..16].copy_from_slice(&(paths.len() as u32).to_le_bytes());
        for (index, path) in paths.iter().enumerate() {
            let base = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN;
            initrd[base..base + path.len()].copy_from_slice(path.as_bytes());
        }
        initrd
    }

    #[test]
    fn entry_paths_lists_every_entry() {
        let table = table_of(&[SORA_INIT_PATH, HELLO_PATH, AUTOEXEC_PATH]);
        let paths: std::vec::Vec<&str> = entry_paths(&table).collect();
        assert_eq!(paths, std::vec![SORA_INIT_PATH, HELLO_PATH, AUTOEXEC_PATH]);
    }

    #[test]
    fn entries_yields_path_and_size() {
        // table_of leaves the len field zero; stamp distinct sizes to prove `entries`
        // reads each entry's `len` field, not the (absent) payload.
        let mut table = table_of(&[SORA_INIT_PATH, HELLO_PATH, AUTOEXEC_PATH]);
        for (index, size) in [4096u64, 15, 73].iter().enumerate() {
            let len_off = INITRD_HEADER_LEN + index * INITRD_ENTRY_LEN + INITRD_PATH_MAX + 8;
            table[len_off..len_off + 8].copy_from_slice(&size.to_le_bytes());
        }
        let listed: std::vec::Vec<(&str, u64)> = entries(&table).collect();
        assert_eq!(
            listed,
            std::vec![
                (SORA_INIT_PATH, 4096u64),
                (HELLO_PATH, 15u64),
                (AUTOEXEC_PATH, 73u64),
            ]
        );
    }

    #[test]
    fn finds_entry_metadata_without_payload() {
        let initrd = one_file(AUTOEXEC_PATH, b"echo cloud\n");
        let table_len = INITRD_HEADER_LEN + INITRD_ENTRY_LEN;
        let entry = find_entry(&initrd[..table_len], AUTOEXEC_PATH)
            .unwrap()
            .unwrap();
        assert_eq!(entry.path, AUTOEXEC_PATH);
        assert_eq!(entry.offset, table_len as u64);
        assert_eq!(entry.len, 11);
        assert_eq!(find_entry(&initrd[..table_len], "missing"), Ok(None));
    }

    #[test]
    fn find_entry_requires_the_complete_table() {
        let table = table_of(&[HELLO_PATH, AUTOEXEC_PATH]);
        assert_eq!(
            find_entry(&table[..INITRD_HEADER_LEN + INITRD_ENTRY_LEN], HELLO_PATH),
            Err(InitrdError::TruncatedTable)
        );
    }

    #[test]
    fn entry_table_bytes_counts_header_and_entries() {
        let table = table_of(&[SORA_INIT_PATH, HELLO_PATH, AUTOEXEC_PATH]);
        assert_eq!(
            entry_table_bytes(&table),
            Ok(INITRD_HEADER_LEN + 3 * INITRD_ENTRY_LEN)
        );
    }

    #[test]
    fn entry_table_bytes_rejects_bad_header() {
        assert_eq!(entry_table_bytes(b"short"), Err(InitrdError::TooSmall));
        let mut table = table_of(&[HELLO_PATH]);
        table[..8].copy_from_slice(b"badmagic");
        assert_eq!(entry_table_bytes(&table), Err(InitrdError::BadMagic));
        table[..8].copy_from_slice(&INITRD_MAGIC);
        table[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(entry_table_bytes(&table), Err(InitrdError::BadVersion));
    }

    #[test]
    fn entry_paths_empty_on_bad_magic_and_truncation() {
        assert_eq!(entry_paths(b"not-an-initrd").count(), 0);
        // A header claiming 2 entries but a buffer holding only one entry's bytes:
        // the second (past the buffer) is skipped, not a panic.
        let mut table = table_of(&[HELLO_PATH, AUTOEXEC_PATH]);
        table[12..16].copy_from_slice(&2u32.to_le_bytes());
        table.truncate(INITRD_HEADER_LEN + INITRD_ENTRY_LEN); // drop the 2nd entry's bytes
        let paths: std::vec::Vec<&str> = entry_paths(&table).collect();
        assert_eq!(paths, std::vec![HELLO_PATH]);
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
