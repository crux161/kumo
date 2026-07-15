//! x86_64 user page-table construction primitives.
//!
//! KUMO is still linked in the low identity map on this bring-up path. Process roots therefore
//! retain PML4[0] as supervisor-only kernel space and reserve PML4[1..256) (512 GiB..128 TiB) for
//! userspace. This prevents the shared 2 MiB architecture-neutral base from aliasing the kernel's
//! low-linked heap while keeping every process in the canonical lower half. — KESTREL

const PAGE_SIZE: u64 = 0x1000;
#[cfg(target_os = "none")]
const TABLE_ENTRIES: usize = 512;

const PRESENT: u64 = 1 << 0;
const WRITE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const NO_EXECUTE: u64 = 1 << 63;
#[cfg(target_os = "none")]
const ADDRESS: u64 = 0x000f_ffff_ffff_f000;

pub const USER_BASE: u64 = 0x0000_0080_0000_0000;
pub const USER_END: u64 = 0x0000_8000_0000_0000;

pub const fn user_page_desc(executable: bool, writable: bool) -> u64 {
    let mut flags = PRESENT | USER;
    if writable {
        flags |= WRITE | NO_EXECUTE;
    } else if !executable {
        flags |= NO_EXECUTE;
    }
    flags
}

pub const fn user_device_page_desc(writable: bool) -> u64 {
    user_page_desc(false, writable) | WRITE_THROUGH | CACHE_DISABLE
}

pub const fn user_nc_page_desc(writable: bool) -> u64 {
    // PAT programming lands with the device/physmap slice. PCD gives the safe uncached
    // first-light behavior without relying on firmware PAT contents.
    user_page_desc(false, writable) | CACHE_DISABLE
}

pub const fn valid_user_range(start: u64, len: u64) -> bool {
    if len == 0 || start < USER_BASE {
        return false;
    }
    match start.checked_add(len) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

#[cfg(target_os = "none")]
pub fn read_root() -> u64 {
    let root: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) root, options(nomem, nostack)) };
    root & ADDRESS
}

#[cfg(not(target_os = "none"))]
pub fn read_root() -> u64 {
    0
}

/// # Safety
/// `root` must identify a live, identity-accessible PML4.
#[cfg(target_os = "none")]
pub unsafe fn set_root(root: u64) {
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack)) };
}

#[cfg(not(target_os = "none"))]
pub unsafe fn set_root(_root: u64) {}

#[cfg(target_os = "none")]
pub fn enable_execute_disable() {
    const EFER: u32 = 0xc000_0080;
    const NXE: u64 = 1 << 11;
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") EFER, out("eax") low, out("edx") high);
        let value = ((high as u64) << 32) | low as u64 | NXE;
        core::arch::asm!(
            "wrmsr",
            in("ecx") EFER,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32
        );
    }
}

#[cfg(target_os = "none")]
fn alloc_table(alloc: &mut dyn FnMut() -> Option<u64>, tables: &mut usize) -> Result<u64, ()> {
    let frame = alloc().ok_or(())?;
    if frame & (PAGE_SIZE - 1) != 0 {
        return Err(());
    }
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
    *tables += 1;
    Ok(frame)
}

/// Create a process PML4, retaining only the low identity kernel entry and any high-half kernel
/// entries from the active root. User PML4 slots start empty.
#[cfg(target_os = "none")]
pub fn create_user_root(
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<u64, ()> {
    let root = alloc_table(alloc, tables)?;
    let active = read_root() as *const u64;
    let process = root as *mut u64;
    unsafe {
        process.write_volatile(active.read_volatile() & !USER);
        let mut index = 256;
        while index < TABLE_ENTRIES {
            process
                .add(index)
                .write_volatile(active.add(index).read_volatile() & !USER);
            index += 1;
        }
    }
    Ok(root)
}

/// Map one 4 KiB userspace page, allocating zeroed intermediate tables as needed.
///
/// # Safety
/// `root`, `pa`, and frames returned by `alloc` must be identity-accessible writable RAM.
#[cfg(target_os = "none")]
pub unsafe fn map_user_page(
    root: u64,
    va: u64,
    pa: u64,
    desc: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<(), ()> {
    if root & (PAGE_SIZE - 1) != 0
        || va & (PAGE_SIZE - 1) != 0
        || pa & (PAGE_SIZE - 1) != 0
        || !valid_user_range(va, PAGE_SIZE)
    {
        return Err(());
    }

    let indices = [
        ((va >> 39) & 0x1ff) as usize,
        ((va >> 30) & 0x1ff) as usize,
        ((va >> 21) & 0x1ff) as usize,
        ((va >> 12) & 0x1ff) as usize,
    ];
    let mut table = root;
    let mut level = 0;
    while level < 3 {
        let entry = unsafe { (table as *mut u64).add(indices[level]) };
        let value = unsafe { entry.read_volatile() };
        table = if value & PRESENT == 0 {
            let child = alloc_table(alloc, tables)?;
            unsafe { entry.write_volatile(child | PRESENT | WRITE | USER) };
            child
        } else {
            if value & USER == 0 {
                return Err(());
            }
            value & ADDRESS
        };
        level += 1;
    }

    let leaf = unsafe { (table as *mut u64).add(indices[3]) };
    if unsafe { leaf.read_volatile() } & PRESENT != 0 {
        return Err(());
    }
    unsafe { leaf.write_volatile((pa & ADDRESS) | desc) };
    Ok(())
}

#[cfg(not(target_os = "none"))]
pub unsafe fn map_user_page(
    _root: u64,
    _va: u64,
    _pa: u64,
    _desc: u64,
    _alloc: &mut dyn FnMut() -> Option<u64>,
    _tables: &mut usize,
) -> Result<(), ()> {
    Ok(())
}

/// Map a 2 MiB user device window as 512 page-granular uncached leaves.
pub unsafe fn map_user_device_block(
    root: u64,
    va: u64,
    pa: u64,
    nc: bool,
    writable: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<(), ()> {
    let desc = if nc {
        user_nc_page_desc(writable)
    } else {
        user_device_page_desc(writable)
    };
    let mut offset = 0;
    while offset < 0x20_0000 {
        unsafe { map_user_page(root, va + offset, pa + offset, desc, alloc, tables)? };
        offset += PAGE_SIZE;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_permissions_enforce_wx() {
        let code = user_page_desc(true, false);
        let data = user_page_desc(false, true);
        let ro = user_page_desc(false, false);
        assert_eq!(code & (PRESENT | USER), PRESENT | USER);
        assert_eq!(code & (WRITE | NO_EXECUTE), 0);
        assert_eq!(data & (WRITE | NO_EXECUTE), WRITE | NO_EXECUTE);
        assert_eq!(ro & WRITE, 0);
        assert_ne!(ro & NO_EXECUTE, 0);
    }

    #[test]
    fn user_arena_excludes_low_kernel_and_upper_half() {
        assert!(!valid_user_range(0x20_0000, PAGE_SIZE));
        assert!(valid_user_range(USER_BASE, PAGE_SIZE));
        assert!(valid_user_range(USER_END - PAGE_SIZE, PAGE_SIZE));
        assert!(!valid_user_range(USER_END, PAGE_SIZE));
        assert!(!valid_user_range(u64::MAX - PAGE_SIZE, PAGE_SIZE * 2));
    }

    #[test]
    fn memory_types_remain_non_executable() {
        assert_ne!(user_device_page_desc(true) & NO_EXECUTE, 0);
        assert_ne!(user_nc_page_desc(false) & NO_EXECUTE, 0);
        assert_ne!(user_device_page_desc(false) & CACHE_DISABLE, 0);
    }
}
