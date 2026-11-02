//j744
//j745
//j751
//! Kernel handle/endpoint adapters for the native Mesa bridge. No FD or physical-address route.
use core::ptr::NonNull;
use kumo_abi::{sys::Syscall, Handle, VmarFlags};
use kumo_gpu::bo::{MapError, Mapper, WaitOutcome};

// The same six-word syscall ABI as kumo-rt, without importing its allocator or startup.
// This adapter can therefore run behind relibc's real C runtime too. — KESTREL
pub(crate) fn call(number: Syscall, args: [u64; 6]) -> (u64, u64) {
    let (first, second): (u64, u64);
    unsafe {
        core::arch::asm!("svc #0", in("x8") number as u64,
            inlateout("x0") args[0] => first, inlateout("x1") args[1] => second,
            in("x2") args[2], in("x3") args[3], in("x4") args[4], in("x5") args[5],
            clobber_abi("C"), options(nostack));
    }
    (first, second)
}
pub struct Memory;
impl Mapper<Handle> for Memory {
    fn barrier(&mut self) {
        // Normal-NC CPU aliases still need store publication and completion ordering.
        // Cache cleaning remains the GPU producer's contract. — MILVUS
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    }
    fn map(&mut self, vmo: &Handle, bytes: u64) -> Result<NonNull<u8>, MapError> {
        let (status, va) = call(
            Syscall::VmarMap,
            [
                0,
                vmo.0 as u64,
                0,
                0,
                bytes,
                (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
            ],
        );
        if status != 0 {
            return Err(MapError::NoMemory);
        }
        NonNull::new(va as *mut u8).ok_or(MapError::Failed)
    }
    fn unmap(&mut self, address: NonNull<u8>, bytes: u64) -> Result<(), MapError> {
        if call(
            Syscall::VmarUnmap,
            [0, address.as_ptr() as u64, bytes, 0, 0, 0],
        )
        .0 != 0
        {
            return Err(MapError::Failed);
        }
        Ok(())
    }
    fn wait(&mut self, wake: &Handle, timeout_ns: u64) -> WaitOutcome {
        let (raw_port, _) = call(Syscall::PortCreate, [0; 6]);
        if raw_port == 0 || raw_port > u32::MAX as u64 {
            return WaitOutcome::Failed;
        }
        let (timer, _) = call(Syscall::TimerCreate, [timeout_ns, 0, 0, 0, 0, 0]);
        if timer == 0 || timer > u32::MAX as u64 {
            call(Syscall::HandleClose, [raw_port, 0, 0, 0, 0, 0]);
            return WaitOutcome::Failed;
        }
        let wake_koid = call(Syscall::HandleKoid, [wake.0 as u64, 0, 0, 0, 0, 0]).0;
        let timer_koid = call(Syscall::HandleKoid, [timer, 0, 0, 0, 0, 0]).0;
        let bound = wake_koid != u64::MAX
            && timer_koid != u64::MAX
            && call(Syscall::PortBind, [raw_port, wake.0 as u64, 0, 0, 0, 0]).0 == 0
            && call(Syscall::PortBind, [raw_port, timer, 0, 0, 0, 0]).0 == 0;
        let result = if bound {
            let source = call(Syscall::PortWait, [raw_port, 0, 0, 0, 0, 0]).0;
            if source == wake_koid {
                let mut byte = [0u8];
                let (len, grant) = call(
                    Syscall::ChannelRead,
                    [wake.0 as u64, byte.as_mut_ptr() as u64, 1, 0, 0, 0],
                );
                // A malformed wake must not leak a transferred handle or imply completion.
                // The owner closes the read end; FenceWait remains authoritative. — MILVUS
                if grant != 0 && grant <= u32::MAX as u64 {
                    call(Syscall::HandleClose, [grant, 0, 0, 0, 0, 0]);
                }
                if len == 1 && byte[0] == 1 && grant == 0 {
                    WaitOutcome::Woken
                } else {
                    WaitOutcome::Closed
                }
            } else if source == timer_koid {
                WaitOutcome::TimedOut
            } else {
                WaitOutcome::Failed
            }
        } else {
            WaitOutcome::Failed
        };
        call(Syscall::PortUnbind, [raw_port, wake.0 as u64, 0, 0, 0, 0]);
        call(Syscall::PortUnbind, [raw_port, timer, 0, 0, 0, 0]);
        call(Syscall::HandleClose, [timer, 0, 0, 0, 0, 0]);
        call(Syscall::HandleClose, [raw_port, 0, 0, 0, 0, 0]);
        result
    }
}
/// Own one brokered GPU endpoint. Close requests containment; it never asserts GPU idle.
/// — KESTREL
pub struct Channel(Option<Handle>);
impl Channel {
    pub fn new(channel: Handle) -> Self {
        Self(Some(channel))
    }
    fn close(&mut self) {
        if let Some(handle) = self.0.take() {
            call(Syscall::HandleClose, [handle.0 as u64, 0, 0, 0, 0, 0]);
        }
    }
}
impl kumo_gpu::client::Transport for Channel {
    type Handle = Handle;
    fn send(
        &mut self,
        bytes: &[u8; kumo_gpu::REQUEST_LEN],
    ) -> Result<(), kumo_gpu::client::TransportError> {
        let channel = self.0.ok_or(kumo_gpu::client::TransportError::Closed)?;
        if call(
            Syscall::ChannelWrite,
            [
                channel.0 as u64,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0,
            ],
        )
        .0 == 0
        {
            Ok(())
        } else {
            Err(kumo_gpu::client::TransportError::Failed)
        }
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; kumo_gpu::REPLY_LEN + 1],
    ) -> Result<(usize, Option<Handle>), kumo_gpu::client::TransportError> {
        let channel = self.0.ok_or(kumo_gpu::client::TransportError::Closed)?;
        let (len, raw) = call(
            Syscall::ChannelRead,
            [
                channel.0 as u64,
                bytes.as_mut_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0,
            ],
        );
        let grant = if raw != 0 && raw <= u32::MAX as u64 {
            Some(Handle(raw as u32))
        } else {
            None
        };
        if len == 0 || (len as i64) < 0 || (raw != 0 && grant.is_none()) {
            if let Some(handle) = grant {
                self.close_handle(handle);
            }
            return Err(kumo_gpu::client::TransportError::Closed);
        }
        Ok((len as usize, grant))
    }
    fn close_handle(&mut self, handle: Handle) {
        call(Syscall::HandleClose, [handle.0 as u64, 0, 0, 0, 0, 0]);
    }
}
impl kumo_gpu::client::SessionTransport for Channel {
    fn close_session(&mut self) {
        self.close();
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}
