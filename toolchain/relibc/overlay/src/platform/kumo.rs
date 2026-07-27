//j505
// Generated from relibc's Pal traits at the revision pinned by KUMO.
// Every method is deliberately explicit: runtime support lands in later slices. — KESTREL

#![allow(deprecated)]

use core::num::NonZeroU64;

use super::{types::*, Pal, PalEpoll, PalPtrace, PalSignal, PalSocket};
use crate::{
    c_str::CStr,
    error::{Errno, Result},
    header::{
        bits_sigset_t::sigset_t,
        signal::{sigaction, sigevent, siginfo_t, sigval, stack_t},
        sys_epoll::epoll_event,
        sys_resource::{rlimit, rusage},
        sys_select::timeval,
        sys_socket::{msghdr, sockaddr, socklen_t},
        sys_stat::stat,
        sys_statvfs::statvfs,
        sys_time::{itimerval, timezone},
        sys_utsname::utsname,
        time::{itimerspec, timespec},
    },
    iter::NulTerminated,
    ld_so::tcb::OsSpecific,
    out::Out,
    pthread,
};

pub struct Sys;

impl Sys {
    pub unsafe fn ioctl(_fd: c_int, _request: c_ulong, _out: *mut c_void) -> Result<c_int> {
        Err(unsupported())
    }
}

const fn unsupported() -> Errno {
    Errno(crate::header::errno::ENOSYS)
}

impl Pal for Sys {
    fn faccessat(fd: c_int, path: CStr, amode: c_int, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn brk(addr: *mut c_void) -> Result<*mut c_void> {
        Err(unsupported())
    }

    fn chdir(path: CStr) -> Result<()> {
        Err(unsupported())
    }

    fn clock_getres(clk_id: clockid_t, tp: Option<Out<timespec>>) -> Result<()> {
        Err(unsupported())
    }

    fn clock_gettime(clk_id: clockid_t, tp: Out<timespec>) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn clock_settime(clk_id: clockid_t, tp: *const timespec) -> Result<()> {
        Err(unsupported())
    }

    fn close(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn dup2(fildes: c_int, fildes2: c_int) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn execve(path: CStr, argv: *const *mut c_char, envp: *const *mut c_char) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn fexecve(
        fildes: c_int,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn exit(status: c_int) -> ! {
        panic!("relibc KUMO stub called: exit")
    }

    unsafe fn exit_thread(stack_base: *mut (), stack_size: usize) -> ! {
        panic!("relibc KUMO stub called: exit_thread")
    }

    fn fchdir(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fchmodat(dirfd: c_int, path: Option<CStr>, mode: mode_t, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fchownat(fildes: c_int, path: CStr, owner: uid_t, group: gid_t, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fdatasync(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn flock(fd: c_int, operation: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fstatat(fildes: c_int, path: Option<CStr>, buf: Out<stat>, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fstatvfs(fildes: c_int, buf: Out<statvfs>) -> Result<()> {
        Err(unsupported())
    }

    fn fcntl(fildes: c_int, cmd: c_int, arg: c_ulonglong) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn fork() -> Result<pid_t> {
        Err(unsupported())
    }

    fn fpath(fildes: c_int, out: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn fsync(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn ftruncate(fildes: c_int, length: off_t) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn futex_wait(addr: *mut u32, val: u32, deadline: Option<&timespec>) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn futex_wake(addr: *mut u32, num: u32) -> Result<u32> {
        Err(unsupported())
    }

    unsafe fn utimensat(
        dirfd: c_int,
        path: CStr,
        times: *const timespec,
        flag: c_int,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getcwd(buf: Out<[u8]>) -> Result<()> {
        Err(unsupported())
    }

    fn getdents(fd: c_int, buf: &mut [u8], opaque_offset: u64) -> Result<usize> {
        Err(unsupported())
    }

    fn dir_seek(fd: c_int, opaque_offset: u64) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn dent_reclen_offset(this_dent: &[u8], offset: usize) -> Option<(u16, u64)> {
        None
    }

    fn getegid() -> gid_t {
        0
    }

    fn geteuid() -> uid_t {
        0
    }

    fn getgid() -> gid_t {
        0
    }

    fn getgroups(list: Out<[gid_t]>) -> Result<c_int> {
        Err(unsupported())
    }

    fn getpagesize() -> usize {
        0
    }

    fn getpgid(pid: pid_t) -> Result<pid_t> {
        Err(unsupported())
    }

    fn getpid() -> pid_t {
        0
    }

    fn getppid() -> pid_t {
        0
    }

    fn getpriority(which: c_int, who: id_t) -> Result<c_int> {
        Err(unsupported())
    }

    fn getrandom(buf: &mut [u8], flags: c_uint) -> Result<usize> {
        Err(unsupported())
    }

    fn getresgid(
        rgid: Option<Out<gid_t>>,
        egid: Option<Out<gid_t>>,
        sgid: Option<Out<gid_t>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getresuid(
        ruid: Option<Out<uid_t>>,
        euid: Option<Out<uid_t>>,
        suid: Option<Out<uid_t>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getrlimit(resource: c_int, rlim: Out<rlimit>) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn setrlimit(resource: c_int, rlim: *const rlimit) -> Result<()> {
        Err(unsupported())
    }

    fn getrusage(who: c_int, r_usage: Out<rusage>) -> Result<()> {
        Err(unsupported())
    }

    fn getsid(pid: pid_t) -> Result<pid_t> {
        Err(unsupported())
    }

    fn gettid() -> pid_t {
        0
    }

    fn gettimeofday(tp: Out<timeval>, tzp: Option<Out<timezone>>) -> Result<()> {
        Err(unsupported())
    }

    fn getuid() -> uid_t {
        0
    }

    fn linkat(fd1: c_int, oldpath: CStr, fd2: c_int, newpath: CStr, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn lseek(fildes: c_int, offset: off_t, whence: c_int) -> Result<off_t> {
        Err(unsupported())
    }

    fn mkdirat(fildes: c_int, path: CStr, mode: mode_t) -> Result<()> {
        Err(unsupported())
    }

    fn mkfifoat(dir_fd: c_int, path: CStr, mode: mode_t) -> Result<()> {
        Err(unsupported())
    }

    fn mknodat(fildes: c_int, path: CStr, mode: mode_t, dev: dev_t) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mlock(addr: *const c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mlockall(flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fildes: c_int,
        off: off_t,
    ) -> Result<*mut c_void> {
        Err(unsupported())
    }

    unsafe fn mremap(
        addr: *mut c_void,
        len: usize,
        new_len: usize,
        flags: c_int,
        args: *mut c_void,
    ) -> Result<*mut c_void> {
        Err(unsupported())
    }

    unsafe fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn msync(addr: *mut c_void, len: usize, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munlock(addr: *const c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn madvise(addr: *mut c_void, len: usize, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munlockall() -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munmap(addr: *mut c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> Result<()> {
        Err(unsupported())
    }

    fn openat(dirfd: c_int, path: CStr, oflag: c_int, mode: mode_t) -> Result<c_int> {
        Err(unsupported())
    }

    fn pipe2(fildes: Out<[c_int; 2]>, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn posix_fallocate(fd: c_int, offset: u64, length: NonZeroU64) -> Result<()> {
        Err(unsupported())
    }

    fn posix_getdents(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn rlct_clone(
        stack: *mut usize,
        os_specific: &mut OsSpecific,
    ) -> Result<pthread::OsTid, Errno> {
        Err(unsupported())
    }

    unsafe fn rlct_kill(os_tid: pthread::OsTid, signal: usize) -> Result<()> {
        Err(unsupported())
    }

    fn current_os_tid() -> pthread::OsTid {
        pthread::OsTid::default()
    }

    fn read(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn pread(fildes: c_int, buf: &mut [u8], offset: off_t) -> Result<usize> {
        Err(unsupported())
    }

    fn readlinkat(dirfd: c_int, pathname: CStr, out: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn renameat2(
        old_dir: c_int,
        old_path: CStr,
        new_dir: c_int,
        new_path: CStr,
        flags: c_uint,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn sched_yield() -> Result<()> {
        Err(unsupported())
    }

    unsafe fn setgroups(size: size_t, list: *const gid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setpgid(pid: pid_t, pgid: pid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setpriority(which: c_int, who: id_t, prio: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn setresgid(rgid: gid_t, egid: gid_t, sgid: gid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setresuid(ruid: uid_t, euid: uid_t, suid: uid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setsid() -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn spawn(
        program: CStr,
        fac: Option<&crate::header::spawn::posix_spawn_file_actions_t>,
        fat: Option<&crate::header::spawn::posix_spawnattr_t>,
        argv: NulTerminated<*mut c_char>,
        envp: Option<NulTerminated<*mut c_char>>,
    ) -> Result<pid_t> {
        Err(unsupported())
    }

    fn symlinkat(path1: CStr, fd: c_int, path2: CStr) -> Result<()> {
        Err(unsupported())
    }

    fn sync() -> Result<()> {
        Err(unsupported())
    }

    fn timer_create(clock_id: clockid_t, evp: &sigevent, timerid: Out<timer_t>) -> Result<()> {
        Err(unsupported())
    }

    fn timer_delete(timerid: timer_t) -> Result<()> {
        Err(unsupported())
    }

    fn timer_gettime(timerid: timer_t, value: Out<itimerspec>) -> Result<()> {
        Err(unsupported())
    }

    fn timer_settime(
        timerid: timer_t,
        flags: c_int,
        value: &itimerspec,
        ovalue: Option<Out<itimerspec>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn umask(mask: mode_t) -> mode_t {
        0
    }

    fn uname(utsname: Out<utsname>) -> Result<()> {
        Err(unsupported())
    }

    fn unlinkat(fd: c_int, path: CStr, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn waitpid(pid: pid_t, stat_loc: Option<Out<c_int>>, options: c_int) -> Result<pid_t> {
        Err(unsupported())
    }

    fn write(fildes: c_int, buf: &[u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn pwrite(fildes: c_int, buf: &[u8], offset: off_t) -> Result<usize> {
        Err(unsupported())
    }

    fn verify() -> bool {
        false
    }
}

impl PalEpoll for Sys {
    fn epoll_create1(flags: c_int) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn epoll_pwait(
        epfd: c_int,
        events: *mut epoll_event,
        maxevents: c_int,
        timeout: c_int,
        sigmask: *const sigset_t,
    ) -> Result<usize> {
        Err(unsupported())
    }
}

impl PalPtrace for Sys {
    unsafe fn ptrace(
        request: c_int,
        pid: pid_t,
        addr: *mut c_void,
        data: *mut c_void,
    ) -> Result<c_int> {
        Err(unsupported())
    }
}

impl PalSignal for Sys {
    fn getitimer(which: c_int, out: &mut itimerval) -> Result<()> {
        Err(unsupported())
    }

    fn kill(pid: pid_t, sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn sigqueue(pid: pid_t, sig: c_int, val: sigval) -> Result<()> {
        Err(unsupported())
    }

    fn killpg(pgrp: pid_t, sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn raise(sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn setitimer(which: c_int, new: &itimerval, old: Option<&mut itimerval>) -> Result<()> {
        Err(unsupported())
    }

    fn sigaction(sig: c_int, act: Option<&sigaction>, oact: Option<&mut sigaction>) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn sigaltstack(ss: Option<&stack_t>, old_ss: Option<&mut stack_t>) -> Result<()> {
        Err(unsupported())
    }

    fn sigpending(set: &mut sigset_t) -> Result<()> {
        Err(unsupported())
    }

    fn sigprocmask(how: c_int, set: Option<&sigset_t>, oset: Option<&mut sigset_t>) -> Result<()> {
        Err(unsupported())
    }

    fn sigsuspend(mask: &sigset_t) -> Errno {
        unsupported()
    }

    fn sigtimedwait(
        set: &sigset_t,
        sig: Option<&mut siginfo_t>,
        tp: Option<&timespec>,
    ) -> Result<c_int> {
        Err(unsupported())
    }
}

impl PalSocket for Sys {
    unsafe fn accept(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn bind(socket: c_int, address: *const sockaddr, address_len: socklen_t) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn connect(
        socket: c_int,
        address: *const sockaddr,
        address_len: socklen_t,
    ) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn getpeername(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn getsockname(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut c_void,
        option_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn listen(socket: c_int, backlog: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn recvfrom(
        socket: c_int,
        buf: *mut c_void,
        len: size_t,
        flags: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn recvmsg(socket: c_int, msg: *mut msghdr, flags: c_int) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn sendmsg(socket: c_int, msg: *const msghdr, flags: c_int) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn sendto(
        socket: c_int,
        buf: *const c_void,
        len: size_t,
        flags: c_int,
        dest_addr: *const sockaddr,
        dest_len: socklen_t,
    ) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn setsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *const c_void,
        option_len: socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn shutdown(socket: c_int, how: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn socket(domain: c_int, kind: c_int, protocol: c_int) -> Result<c_int> {
        Err(unsupported())
    }

    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: &mut [c_int; 2]) -> Result<()> {
        Err(unsupported())
    }
}
