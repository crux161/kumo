//j456

//! General frame-backed x86_64 userspace image construction and synchronous entry.

#[cfg(target_os = "none")]
use crate::UserLoadSegment;
use crate::{El0Report, UserImage, UserImageError};

const PAGE_SIZE: u64 = 0x1000;
const FIRST_LIGHT_BASE: u64 = crate::paging::USER_BASE;
const FIRST_LIGHT_STACK_TOP: u64 = 0x0000_0100_0000_0000;
const FIRST_LIGHT_STACK_SIZE: u64 = PAGE_SIZE;

fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

fn align_up(value: u64) -> Option<u64> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
}

#[cfg(target_os = "none")]
fn alloc_data_frame(alloc: &mut dyn FnMut() -> Option<u64>) -> Result<u64, UserImageError> {
    let frame = alloc().ok_or(UserImageError::OutOfFrames)?;
    if frame & (PAGE_SIZE - 1) != 0 {
        return Err(UserImageError::OutOfFrames);
    }
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
    Ok(frame)
}

#[cfg(target_os = "none")]
pub fn build_user_tables(
    image: &UserImage<'_>,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<u64, UserImageError> {
    if image.segments.is_empty() && image.extra_mappings.is_empty() {
        return Err(UserImageError::Empty);
    }

    crate::paging::enable_execute_disable();
    let mut tables = 0;
    let root = crate::paging::create_user_root(alloc, &mut tables)
        .map_err(|()| UserImageError::OutOfFrames)?;

    for segment in image.segments {
        if segment.writable && segment.executable || segment.source.len() as u64 > segment.mem_size
        {
            return Err(UserImageError::BadSegment);
        }
        if segment.mem_size == 0 {
            continue;
        }
        let segment_end = segment
            .virt_addr
            .checked_add(segment.mem_size)
            .ok_or(UserImageError::BadSegment)?;
        if !crate::paging::valid_user_range(segment.virt_addr, segment.mem_size) {
            return Err(UserImageError::SegmentOutsideImageBlock);
        }
        let source_end = segment.virt_addr + segment.source.len() as u64;
        let desc = crate::paging::user_page_desc(segment.executable, segment.writable);
        let mut page = align_down(segment.virt_addr);
        let last = align_down(segment_end - 1);
        loop {
            let frame = alloc_data_frame(alloc)?;
            let copy_start = core::cmp::max(page, segment.virt_addr);
            let copy_end = core::cmp::min(page + PAGE_SIZE, source_end);
            if copy_end > copy_start {
                let source_offset = (copy_start - segment.virt_addr) as usize;
                let frame_offset = (copy_start - page) as usize;
                let len = (copy_end - copy_start) as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        segment.source.as_ptr().add(source_offset),
                        (frame as *mut u8).add(frame_offset),
                        len,
                    );
                }
            }
            unsafe { crate::paging::map_user_page(root, page, frame, desc, alloc, &mut tables) }
                .map_err(|()| UserImageError::BadSegment)?;
            if page == last {
                break;
            }
            page += PAGE_SIZE;
        }
    }

    let stack_size = align_up(image.stack_size).ok_or(UserImageError::BadStack)?;
    if stack_size == 0 || image.stack_top & (PAGE_SIZE - 1) != 0 || stack_size > image.stack_top {
        return Err(UserImageError::BadStack);
    }
    let stack_base = image.stack_top - stack_size;
    if !crate::paging::valid_user_range(stack_base, stack_size) {
        return Err(UserImageError::StackOutsideStackBlock);
    }
    let stack_desc = crate::paging::user_page_desc(false, true);
    let mut page = stack_base;
    while page < image.stack_top {
        let frame = alloc_data_frame(alloc)?;
        unsafe { crate::paging::map_user_page(root, page, frame, stack_desc, alloc, &mut tables) }
            .map_err(|()| UserImageError::BadStack)?;
        page += PAGE_SIZE;
    }
    // The page immediately below `stack_base` is deliberately left unmapped as a guard.

    for mapping in image.extra_mappings {
        if mapping.len == 0
            || mapping.writable && mapping.executable
            || mapping.device && mapping.uncached
            || mapping.phys_base & (PAGE_SIZE - 1) != 0
            || mapping.virt_addr & (PAGE_SIZE - 1) != 0
            || !crate::paging::valid_user_range(mapping.virt_addr, mapping.len)
        {
            return Err(UserImageError::BadSegment);
        }
        let desc = if mapping.device {
            crate::paging::user_device_page_desc(mapping.writable)
        } else if mapping.uncached {
            crate::paging::user_nc_page_desc(mapping.writable)
        } else {
            crate::paging::user_page_desc(mapping.executable, mapping.writable)
        };
        let pages = mapping.len.div_ceil(PAGE_SIZE);
        if mapping
            .phys_base
            .checked_add(
                pages
                    .checked_mul(PAGE_SIZE)
                    .ok_or(UserImageError::ImageTooLarge)?,
            )
            .is_none()
        {
            return Err(UserImageError::ImageTooLarge);
        }
        let mut index = 0;
        while index < pages {
            let offset = index * PAGE_SIZE;
            unsafe {
                crate::paging::map_user_page(
                    root,
                    mapping.virt_addr + offset,
                    mapping.phys_base + offset,
                    desc,
                    alloc,
                    &mut tables,
                )
            }
            .map_err(|()| UserImageError::BadSegment)?;
            index += 1;
        }
    }

    Ok(root)
}

#[cfg(not(target_os = "none"))]
pub fn build_user_tables(
    _image: &UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<u64, UserImageError> {
    Err(UserImageError::Unsupported)
}

#[cfg(target_os = "none")]
pub fn run_el0_image(
    image: UserImage<'_>,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    if !crate::paging::valid_user_range(image.entry, 1) {
        return Err(UserImageError::SegmentOutsideImageBlock);
    }
    let root = build_user_tables(&image, alloc)?;
    let kernel_root = crate::paging::read_root();
    crate::ring3::reset_counters();
    unsafe { crate::paging::set_root(root) };
    let exit_code =
        unsafe { crate::ring3::enter(image.entry, image.stack_top - 16, image.bootstrap) };
    unsafe { crate::paging::set_root(kernel_root) };

    Ok(El0Report {
        entered: true,
        syscalls: crate::ring3::syscall_count(),
        ping_echo: crate::ring3::ping_echo(),
        exit_code,
    })
}

#[cfg(not(target_os = "none"))]
pub fn run_el0_image(
    _image: UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    Err(UserImageError::Unsupported)
}

pub fn run_el0_smoke(
    base: u64,
    stack_top: u64,
    stack_size: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    #[cfg(target_os = "none")]
    {
        let segment = UserLoadSegment {
            source: crate::ring3::payload(),
            virt_addr: base,
            mem_size: crate::ring3::payload().len() as u64,
            writable: false,
            executable: true,
        };
        let image = UserImage {
            entry: base,
            stack_top,
            stack_size,
            bootstrap: 0,
            segments: core::slice::from_ref(&segment),
            extra_mappings: &[],
        };
        run_el0_image(image, alloc)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (base, stack_top, stack_size, alloc);
        Err(UserImageError::Unsupported)
    }
}

/// Build a user image in a private address space without entering it. The kernel's shared
/// `user_thread` dispatcher consumes the returned state, so compiler-produced ELF images and
/// first-light probes use exactly the same scheduled CPL3 path. Physical ownership stays in
/// the kernel: callers supply the validated BootInfo-backed frame allocator.
#[cfg(target_os = "none")]
pub fn prepare_scheduled_user_image(
    image: &UserImage<'_>,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::UserState, UserImageError> {
    let root = build_user_tables(image, alloc)?;
    crate::ring3::reset_counters();
    let mut x = [0; 31];
    x[0] = image.bootstrap;
    Ok(crate::UserState {
        x,
        elr: image.entry,
        spsr: 0,
        sp_el0: image.stack_top - 16,
        ttbr0: root,
    })
}

#[cfg(not(target_os = "none"))]
pub fn prepare_scheduled_user_image(
    _image: &UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::UserState, UserImageError> {
    Err(UserImageError::Unsupported)
}

/// First-light wrapper retained for the synchronous ring-3 diagnostic and host API parity.
pub fn prepare_scheduled_smoke(
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::UserState, UserImageError> {
    #[cfg(target_os = "none")]
    {
        let segment = UserLoadSegment {
            source: crate::ring3::payload(),
            virt_addr: FIRST_LIGHT_BASE,
            mem_size: crate::ring3::payload().len() as u64,
            writable: false,
            executable: true,
        };
        let image = UserImage {
            entry: FIRST_LIGHT_BASE,
            stack_top: FIRST_LIGHT_STACK_TOP,
            stack_size: FIRST_LIGHT_STACK_SIZE,
            bootstrap: 0,
            segments: core::slice::from_ref(&segment),
            extra_mappings: &[],
        };
        prepare_scheduled_user_image(&image, alloc)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = alloc;
        Err(UserImageError::Unsupported)
    }
}

/// Build the private address space for one side of the two-context CPL3 FP/SIMD ownership proof.
/// The payload loads `sentinel` into XMM0, yields through `int 0x80`, and checks it after resume.
pub fn prepare_scheduled_fpsimd_smoke(
    sentinel: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::UserState, UserImageError> {
    #[cfg(target_os = "none")]
    {
        let segment = UserLoadSegment {
            source: crate::ring3::fpsimd_payload(),
            virt_addr: FIRST_LIGHT_BASE,
            mem_size: crate::ring3::fpsimd_payload().len() as u64,
            writable: false,
            executable: true,
        };
        let image = UserImage {
            entry: FIRST_LIGHT_BASE,
            stack_top: FIRST_LIGHT_STACK_TOP,
            stack_size: FIRST_LIGHT_STACK_SIZE,
            bootstrap: sentinel,
            segments: core::slice::from_ref(&segment),
            extra_mappings: &[],
        };
        prepare_scheduled_user_image(&image, alloc)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (sentinel, alloc);
        Err(UserImageError::Unsupported)
    }
}

#[cfg(target_os = "none")]
pub fn first_light_smoke(
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::Ring3Report, crate::Ring3Error> {
    let report = run_el0_smoke(
        FIRST_LIGHT_BASE,
        FIRST_LIGHT_STACK_TOP,
        FIRST_LIGHT_STACK_SIZE,
        alloc,
    )
    .map_err(crate::Ring3Error::UserImage)?;
    Ok(crate::Ring3Report {
        entered: report.entered,
        calls: report.syscalls,
        ping_echo: report.ping_echo,
        exit_code: report.exit_code,
        code_address: FIRST_LIGHT_BASE,
        stack_top: FIRST_LIGHT_STACK_TOP,
    })
}

#[cfg(not(target_os = "none"))]
pub fn first_light_smoke(
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<crate::Ring3Report, crate::Ring3Error> {
    Err(crate::Ring3Error::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_helpers_cover_partial_pages() {
        assert_eq!(align_down(0x1234), 0x1000);
        assert_eq!(align_up(0x1234), Some(0x2000));
        assert_eq!(align_up(0x2000), Some(0x2000));
        assert_eq!(align_up(u64::MAX), None);
    }

    #[test]
    fn first_light_addresses_live_in_x86_user_arena() {
        assert!(crate::paging::valid_user_range(FIRST_LIGHT_BASE, PAGE_SIZE));
        assert!(crate::paging::valid_user_range(
            FIRST_LIGHT_STACK_TOP - FIRST_LIGHT_STACK_SIZE,
            FIRST_LIGHT_STACK_SIZE
        ));
    }
}
