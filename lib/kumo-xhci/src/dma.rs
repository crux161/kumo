//j539

//! Volatile-only access to memory the controller writes.
//!
//! A Rust `&mut [u8]` or `&[Trb]` over DMA memory is a promise to the optimizer that nothing
//! outside the program changes those bytes while the reference lives. The controller does exactly
//! that, invisibly. LLVM may then forward the zeros it just stored into a buffer instead of loading
//! what the device wrote, sink that zero-fill past the doorbell, or hoist an event-ring load out of
//! a polling loop. Whether it does depends on inlining, so a refactor can flip a working driver into
//! one that reads silent zeros: J538 lost three boots to hub descriptors of 16 zero bytes while the
//! hub had sent exactly 12 and 9.
//!
//! The types here hold a raw pointer and never hand out a reference to the bytes. The only way data
//! enters or leaves is a volatile access, and parsers get a copy.

use crate::Trb;

/// A span of DMA memory reached only through volatile accesses.
///
/// `Copy`, so a driver can hand the same window to the transfer that fills it and the code that
/// reads it, without a borrow that would re-create the aliasing promise.
#[derive(Clone, Copy, Debug)]
pub struct DmaBuf {
    ptr: *mut u8,
    phys: u64,
    len: usize,
}

impl DmaBuf {
    /// # Safety
    ///
    /// `ptr..ptr + len` must stay mapped and device-visible at `phys` for as long as any copy of
    /// this value is used, and no Rust reference to those bytes may exist at the same time.
    pub const unsafe fn new(ptr: *mut u8, phys: u64, len: usize) -> Self {
        Self { ptr, phys, len }
    }

    /// The address the controller uses for the first byte.
    pub const fn phys(self) -> u64 {
        self.phys
    }

    pub const fn len(self) -> usize {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The sub-span `[offset, offset + len)`, or `None` if it does not fit.
    pub fn window(self, offset: usize, len: usize) -> Option<DmaBuf> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }
        Some(DmaBuf {
            // SAFETY: `offset <= end <= self.len`, so the result stays inside this span.
            ptr: unsafe { self.ptr.add(offset) },
            phys: self.phys + offset as u64,
            len,
        })
    }

    /// Zero every byte with volatile stores, so the fill cannot be moved past the doorbell that
    /// hands the buffer to the device.
    pub fn zero(self) {
        for index in 0..self.len {
            // SAFETY: `index < len`; the constructor's contract covers the span.
            unsafe { self.ptr.add(index).write_volatile(0) };
        }
    }

    /// The byte at `offset`. Panics past the end: a driver bug, not a device fault.
    pub fn byte(self, offset: usize) -> u8 {
        assert!(offset < self.len, "DmaBuf byte {offset} past {}", self.len);
        // SAFETY: bounds checked above.
        unsafe { self.ptr.add(offset).read_volatile() }
    }

    /// Copy `out.len()` bytes starting at `offset` out of the device's memory.
    pub fn read_into(self, offset: usize, out: &mut [u8]) {
        let end = offset.checked_add(out.len());
        assert!(
            end.is_some_and(|end| end <= self.len),
            "DmaBuf read {offset}+{} past {}",
            out.len(),
            self.len
        );
        for (index, byte) in out.iter_mut().enumerate() {
            // SAFETY: bounds checked above.
            *byte = unsafe { self.ptr.add(offset + index).read_volatile() };
        }
    }

    /// `N` bytes starting at `offset`, copied out.
    pub fn read<const N: usize>(self, offset: usize) -> [u8; N] {
        let mut out = [0u8; N];
        self.read_into(offset, &mut out);
        out
    }

    /// Copy `bytes` in at `offset` with volatile stores, for buffers the device reads.
    pub fn write_from(self, offset: usize, bytes: &[u8]) {
        let end = offset.checked_add(bytes.len());
        assert!(
            end.is_some_and(|end| end <= self.len),
            "DmaBuf write {offset}+{} past {}",
            bytes.len(),
            self.len
        );
        for (index, byte) in bytes.iter().enumerate() {
            // SAFETY: bounds checked above.
            unsafe { self.ptr.add(offset + index).write_volatile(*byte) };
        }
    }
}

/// Where an event ring's TRBs are read from. The driver reads controller-written memory through
/// [`DmaTrbs`]; host tests use a slice.
pub trait TrbSource {
    fn trb_count(&self) -> usize;
    /// The TRB at `index`. Implementations over device memory must read the dword holding the
    /// Cycle bit first: the controller writes it last, so the other three are only valid once it
    /// has changed.
    fn read_trb(&self, index: usize) -> Trb;
}

impl TrbSource for [Trb] {
    fn trb_count(&self) -> usize {
        self.len()
    }

    fn read_trb(&self, index: usize) -> Trb {
        self[index]
    }
}

impl<const N: usize> TrbSource for [Trb; N] {
    fn trb_count(&self) -> usize {
        N
    }

    fn read_trb(&self, index: usize) -> Trb {
        self[index]
    }
}

/// An event-ring segment in device memory, read only through volatile loads.
#[derive(Clone, Copy, Debug)]
pub struct DmaTrbs {
    ptr: *const Trb,
    count: usize,
}

impl DmaTrbs {
    /// # Safety
    ///
    /// `ptr` must point at `count` mapped, 16-byte-aligned TRBs that stay valid while any copy of
    /// this value is used, with no Rust reference to them alive at the same time.
    pub const unsafe fn new(ptr: *const Trb, count: usize) -> Self {
        Self { ptr, count }
    }
}

impl TrbSource for DmaTrbs {
    fn trb_count(&self) -> usize {
        self.count
    }

    fn read_trb(&self, index: usize) -> Trb {
        assert!(
            index < self.count,
            "DmaTrbs index {index} past {}",
            self.count
        );
        // SAFETY: bounds checked; a TRB is four naturally aligned u32 words.
        let words = unsafe { self.ptr.add(index) } as *const u32;
        // Cycle dword first; the barrier keeps the remaining loads from being satisfied earlier.
        let control = unsafe { words.add(3).read_volatile() };
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let mut out = [0u32; 4];
        for (word, slot) in out.iter_mut().take(3).enumerate() {
            // SAFETY: `word < 3`, inside this TRB.
            *slot = unsafe { words.add(word).read_volatile() };
        }
        out[3] = control;
        Trb::from_words(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_over(bytes: &mut [u8]) -> DmaBuf {
        unsafe { DmaBuf::new(bytes.as_mut_ptr(), 0x8000_0000, bytes.len()) }
    }

    #[test]
    fn zero_read_and_write_go_through_the_span() {
        let mut backing = [0xaau8; 32];
        let buf = buf_over(&mut backing);
        buf.write_from(4, &[1, 2, 3]);
        assert_eq!(buf.read::<3>(4), [1, 2, 3]);
        assert_eq!(buf.byte(5), 2);
        buf.zero();
        assert_eq!(buf.read::<32>(0), [0u8; 32]);
    }

    #[test]
    fn a_window_keeps_its_device_address_and_bounds() {
        let mut backing = [0u8; 64];
        let buf = buf_over(&mut backing);
        let csw = buf.window(48, 13).unwrap();
        assert_eq!(csw.phys(), 0x8000_0030);
        assert_eq!(csw.len(), 13);
        csw.write_from(0, b"USBS");
        assert_eq!(buf.read::<4>(48), *b"USBS");
        assert!(buf.window(60, 8).is_none());
        assert!(buf.window(usize::MAX, 2).is_none());
    }

    #[test]
    #[should_panic(expected = "past")]
    fn reading_past_the_end_is_a_loud_driver_bug() {
        let mut backing = [0u8; 8];
        buf_over(&mut backing).read::<4>(6);
    }

    #[test]
    fn device_memory_trbs_read_like_a_slice() {
        let ring = [
            Trb::from_words([1, 2, 3, 0x0000_8401]),
            Trb::from_words([5, 6, 7, 0x0000_8400]),
        ];
        let source = unsafe { DmaTrbs::new(ring.as_ptr(), ring.len()) };
        assert_eq!(source.trb_count(), 2);
        for index in 0..2 {
            assert_eq!(source.read_trb(index), ring.as_slice().read_trb(index));
        }
    }
}
