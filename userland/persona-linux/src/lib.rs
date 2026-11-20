#![no_std]

//j741
//j885
//j888
//j890
//j915

pub mod stream;

/// ARM64 Linux syscall numbers used by the M10 MVP.
///
/// This crate is intentionally tiny: it names only the syscall surface needed
/// to prove the first static arm64 Linux payload. More numbers should be added
/// only when a concrete failing binary needs them.
pub mod arm64 {
    pub const OPENAT: u64 = 56;
    pub const CLOSE: u64 = 57;
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const WRITEV: u64 = 66;
    pub const NEWFSTATAT: u64 = 79;
    pub const EXIT: u64 = 93;
    pub const EXIT_GROUP: u64 = 94;
    pub const MUNMAP: u64 = 215;
    pub const BRK: u64 = 214;
    pub const MMAP: u64 = 222;

    pub const STDOUT: u64 = 1;
    pub const STDERR: u64 = 2;
}

pub mod elf {
    pub const ELF_HEADER_LEN: usize = 64;
    pub const ELF_PHDR_LEN: usize = 56;
    pub const ET_EXEC: u16 = 2;
    /// SH7-c: a position-independent executable or a shared object (musl's loader is one).
    pub const ET_DYN: u16 = 3;
    pub const EM_X86_64: u16 = 0x3e;
    pub const EM_AARCH64: u16 = 0xb7;
    pub const PT_LOAD: u32 = 1;
    /// SH7: a dynamic executable names its loader here (PLAN/022 SH7, payload P1).
    pub const PT_INTERP: u32 = 3;
    /// SH7-c: where the program headers sit in memory; a dynamic loader finds its program by it.
    pub const PT_PHDR: u32 = 6;
    //j741
    /// The stack's program header. Its `p_memsz` is the stack size the program asks for
    /// (`lld -z stack-size=`); zero means "the loader's default". — TIMBERDOODLE
    pub const PT_GNU_STACK: u32 = 0x6474_e551;
    pub const PF_X: u32 = 1 << 0;
    pub const PF_W: u32 = 1 << 1;
    pub const PF_R: u32 = 1 << 2;

    /// ELF machine accepted by the persona loader. Callers select this explicitly so an
    /// architecture-specific runner cannot accidentally launch another architecture's image.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Machine {
        X86_64,
        Aarch64,
    }

    impl Machine {
        const fn elf_id(self) -> u16 {
            match self {
                Self::X86_64 => EM_X86_64,
                Self::Aarch64 => EM_AARCH64,
            }
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum ElfError {
        TooSmall,
        BadMagic,
        NotElf64,
        NotLittleEndian,
        NotExecutable,
        WrongMachine,
        BadProgramHeaderSize,
        SegmentMemSmallerThanFile,
        /// `PT_INTERP` is empty or its path has no terminating NUL.
        BadInterpreter,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ElfHeader {
        /// `ET_DYN`: link addresses are relative and the loader picks a bias (SH7-c).
        pub position_independent: bool,
        pub entry: u64,
        pub phoff: u64,
        pub phentsize: u16,
        pub phnum: u16,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ProgramHeader {
        pub kind: u32,
        pub flags: u32,
        pub file_offset: u64,
        pub virt_addr: u64,
        pub file_size: u64,
        pub mem_size: u64,
    }

    /// Parse the established M10 ARM64 ELF contract.
    ///
    /// This compatibility entry point remains ARM64-only. AMD64 runners must opt in through
    /// [`parse_header_for_machine`] so extending the parser does not widen existing callers.
    pub fn parse_header(bytes: &[u8]) -> Result<ElfHeader, ElfError> {
        parse_header_for_machine(bytes, Machine::Aarch64)
    }

    /// Parse an executable ELF64 header for exactly `machine`.
    pub fn parse_header_for_machine(bytes: &[u8], machine: Machine) -> Result<ElfHeader, ElfError> {
        if bytes.len() < ELF_HEADER_LEN {
            return Err(ElfError::TooSmall);
        }
        if bytes[0..4] != *b"\x7fELF" {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != 2 {
            return Err(ElfError::NotElf64);
        }
        if bytes[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        if read_u16(bytes, 16)? != ET_EXEC {
            return Err(ElfError::NotExecutable);
        }
        parse_checked_header(bytes, machine, false)
    }

    /// Parse a Linux program image for the persona loader: a fixed-address executable
    /// (`ET_EXEC`) or a position-independent one (`ET_DYN`, which is also what a dynamic
    /// loader such as musl's `ld-musl-aarch64.so.1` is). KUMO's own loader keeps
    /// [`parse_header`], which refuses `ET_DYN`. — TEAL (SH7-c)
    pub fn parse_linux_image(bytes: &[u8], machine: Machine) -> Result<ElfHeader, ElfError> {
        if bytes.len() < ELF_HEADER_LEN {
            return Err(ElfError::TooSmall);
        }
        if bytes[0..4] != *b"\x7fELF" {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != 2 {
            return Err(ElfError::NotElf64);
        }
        if bytes[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        let position_independent = match read_u16(bytes, 16)? {
            ET_EXEC => false,
            ET_DYN => true,
            _ => return Err(ElfError::NotExecutable),
        };
        parse_checked_header(bytes, machine, position_independent)
    }

    fn parse_checked_header(
        bytes: &[u8],
        machine: Machine,
        position_independent: bool,
    ) -> Result<ElfHeader, ElfError> {
        if read_u16(bytes, 18)? != machine.elf_id() {
            return Err(ElfError::WrongMachine);
        }
        let phentsize = read_u16(bytes, 54)?;
        if phentsize as usize != ELF_PHDR_LEN {
            return Err(ElfError::BadProgramHeaderSize);
        }

        Ok(ElfHeader {
            position_independent,
            entry: read_u64(bytes, 24)?,
            phoff: read_u64(bytes, 32)?,
            phentsize,
            phnum: read_u16(bytes, 56)?,
        })
    }

    pub fn parse_program_header(bytes: &[u8]) -> Result<ProgramHeader, ElfError> {
        if bytes.len() < ELF_PHDR_LEN {
            return Err(ElfError::TooSmall);
        }
        let file_size = read_u64(bytes, 32)?;
        let mem_size = read_u64(bytes, 40)?;
        if mem_size < file_size {
            return Err(ElfError::SegmentMemSmallerThanFile);
        }

        Ok(ProgramHeader {
            kind: read_u32(bytes, 0)?,
            flags: read_u32(bytes, 4)?,
            file_offset: read_u64(bytes, 8)?,
            virt_addr: read_u64(bytes, 16)?,
            file_size,
            mem_size,
        })
    }

    /// The `index`th program header of `image`.
    pub fn program_header(
        image: &[u8],
        header: &ElfHeader,
        index: u16,
    ) -> Result<ProgramHeader, ElfError> {
        let at = (index as u64)
            .checked_mul(header.phentsize as u64)
            .and_then(|offset| offset.checked_add(header.phoff))
            .ok_or(ElfError::TooSmall)? as usize;
        let end = at.checked_add(ELF_PHDR_LEN).ok_or(ElfError::TooSmall)?;
        parse_program_header(image.get(at..end).ok_or(ElfError::TooSmall)?)
    }

    /// The interpreter a dynamic program names in `PT_INTERP`, without its NUL, or `None` for a
    /// static program. A path that is empty, unterminated or outside the image is an error.
    pub fn interpreter<'a>(
        image: &'a [u8],
        header: &ElfHeader,
    ) -> Result<Option<&'a [u8]>, ElfError> {
        for index in 0..header.phnum {
            let ph = program_header(image, header, index)?;
            if ph.kind != PT_INTERP {
                continue;
            }
            let start = ph.file_offset as usize;
            let end = start
                .checked_add(ph.file_size as usize)
                .ok_or(ElfError::TooSmall)?;
            let raw = image.get(start..end).ok_or(ElfError::TooSmall)?;
            let len = raw
                .iter()
                .position(|b| *b == 0)
                .ok_or(ElfError::BadInterpreter)?;
            if len == 0 {
                return Err(ElfError::BadInterpreter);
            }
            return Ok(Some(&raw[..len]));
        }
        Ok(None)
    }

    fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
        let end = offset.checked_add(2).ok_or(ElfError::TooSmall)?;
        if end > bytes.len() {
            return Err(ElfError::TooSmall);
        }
        Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
    }

    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
        let end = offset.checked_add(4).ok_or(ElfError::TooSmall)?;
        if end > bytes.len() {
            return Err(ElfError::TooSmall);
        }
        Ok(u32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]))
    }

    fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
        let end = offset.checked_add(8).ok_or(ElfError::TooSmall)?;
        if end > bytes.len() {
            return Err(ElfError::TooSmall);
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

        fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
            buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }

        fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
            buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
            buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn exec_header(machine: u16, entry: u64) -> [u8; ELF_HEADER_LEN] {
            let mut header = [0u8; ELF_HEADER_LEN];
            header[0..4].copy_from_slice(b"\x7fELF");
            header[4] = 2;
            header[5] = 1;
            put_u16(&mut header, 16, ET_EXEC);
            put_u16(&mut header, 18, machine);
            put_u64(&mut header, 24, entry);
            put_u64(&mut header, 32, ELF_HEADER_LEN as u64);
            put_u16(&mut header, 54, ELF_PHDR_LEN as u16);
            put_u16(&mut header, 56, 1);
            header
        }

        #[test]
        fn parses_aarch64_exec_header_and_load_segment() {
            let header = exec_header(EM_AARCH64, 0x1000_1000);

            let parsed = parse_header(&header).unwrap();
            assert_eq!(parsed.entry, 0x1000_1000);
            assert_eq!(parsed.phnum, 1);

            let mut ph = [0u8; ELF_PHDR_LEN];
            put_u32(&mut ph, 0, PT_LOAD);
            put_u32(&mut ph, 4, PF_R | PF_X);
            put_u64(&mut ph, 8, 0x1000);
            put_u64(&mut ph, 16, 0x1000_1000);
            put_u64(&mut ph, 32, 36);
            put_u64(&mut ph, 40, 36);
            let segment = parse_program_header(&ph).unwrap();
            assert_eq!(segment.kind, PT_LOAD);
            assert_eq!(segment.flags, PF_R | PF_X);
            assert_eq!(segment.file_offset, 0x1000);
        }

        #[test]
        fn parses_x86_64_exec_header_only_when_requested() {
            let header = exec_header(EM_X86_64, 0x0040_1000);

            assert_eq!(parse_header(&header), Err(ElfError::WrongMachine));
            let parsed = parse_header_for_machine(&header, Machine::X86_64).unwrap();
            assert_eq!(parsed.entry, 0x0040_1000);
            assert_eq!(parsed.phnum, 1);
        }

        //j888
        /// SH7-c: a PIE is a Linux image but never a KUMO one; the machine check still applies.
        #[test]
        fn et_dyn_is_a_linux_image_only() {
            let mut header = exec_header(EM_AARCH64, 0x6ac);
            put_u16(&mut header, 16, ET_DYN);
            assert_eq!(parse_header(&header), Err(ElfError::NotExecutable));
            let parsed = parse_linux_image(&header, Machine::Aarch64).unwrap();
            assert!(parsed.position_independent);
            assert_eq!(parsed.entry, 0x6ac);
            assert_eq!(
                parse_linux_image(&header, Machine::X86_64),
                Err(ElfError::WrongMachine)
            );
            let fixed = parse_linux_image(&exec_header(EM_AARCH64, 0x1000), Machine::Aarch64);
            assert!(!fixed.unwrap().position_independent);
        }

        /// The vendored payloads (persona/, built by scripts/persona-payloads.sh): the hello names
        /// musl's loader, and the loader is a position-independent image with no loader of its own.
        #[test]
        fn the_vendored_hello_names_musl_and_musl_needs_no_interpreter() {
            let hello = include_bytes!("../../../persona/hello-dyn");
            let header = parse_linux_image(hello, Machine::Aarch64).unwrap();
            assert!(header.position_independent);
            assert_eq!(
                interpreter(hello, &header),
                Ok(Some(&b"/lib/ld-musl-aarch64.so.1"[..]))
            );
            let phdr = (0..header.phnum)
                .map(|i| program_header(hello, &header, i).unwrap())
                .find(|ph| ph.kind == PT_PHDR)
                .expect("a PIE carries PT_PHDR");
            assert_eq!(phdr.virt_addr, header.phoff);

            let musl = include_bytes!("../../../persona/musl/ld-musl-aarch64.so.1");
            let header = parse_linux_image(musl, Machine::Aarch64).unwrap();
            assert!(header.position_independent);
            assert_eq!(interpreter(musl, &header), Ok(None));
        }

        #[test]
        fn a_malformed_interpreter_is_refused() {
            // One PT_INTERP header whose 4-byte path has no NUL.
            let mut image = [0u8; ELF_HEADER_LEN + ELF_PHDR_LEN + 4];
            image[..ELF_HEADER_LEN].copy_from_slice(&exec_header(EM_AARCH64, 0));
            let ph = ELF_HEADER_LEN;
            put_u32(&mut image, ph, PT_INTERP);
            put_u64(&mut image, ph + 8, (ELF_HEADER_LEN + ELF_PHDR_LEN) as u64);
            put_u64(&mut image, ph + 32, 4);
            put_u64(&mut image, ph + 40, 4);
            image[ELF_HEADER_LEN + ELF_PHDR_LEN..].copy_from_slice(b"/lib");
            let header = parse_header(&image).unwrap();
            assert_eq!(interpreter(&image, &header), Err(ElfError::BadInterpreter));
            // A path that runs off the end of the image.
            put_u64(&mut image, ph + 32, 400);
            put_u64(&mut image, ph + 40, 400);
            assert_eq!(interpreter(&image, &header), Err(ElfError::TooSmall));
        }

        #[test]
        fn explicit_machine_rejects_cross_arch_images() {
            let arm64 = exec_header(EM_AARCH64, 0x1000_1000);
            let x86_64 = exec_header(EM_X86_64, 0x0040_1000);

            assert_eq!(
                parse_header_for_machine(&arm64, Machine::X86_64),
                Err(ElfError::WrongMachine)
            );
            assert_eq!(
                parse_header_for_machine(&x86_64, Machine::Aarch64),
                Err(ElfError::WrongMachine)
            );
        }
    }
}
