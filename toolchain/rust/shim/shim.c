// j853
// j871
// j873
// The KUMO std shim: the few C symbols std references that relibc does not provide.
// Freestanding C, no headers. Everything outside the surface below refuses by name
// (ENOSYS) rather than pretending.
typedef unsigned long usize;

#define ENOSYS 38
#define EAGAIN 11

#define SYS_FUTEX 98
#define SYS_GETRANDOM 278
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_WAIT_BITSET 9
#define FUTEX_WAKE_BITSET 10

// KUMO kernel objects (userland/kumo-rt/src/sys.rs): FutexWait(addr, expected),
// FutexWake(addr, count).
#define SYSCALL_FUTEX_WAIT 28
#define SYSCALL_FUTEX_WAKE 29
#define SYSCALL_CLOCK_GET 26

static long svc2(long n, long a0, long a1) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a0;
    register long x1 __asm__("x1") = a1;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1) : "memory");
    return x0;
}

static unsigned long clock_now(void) {
    return (unsigned long)svc2(SYSCALL_CLOCK_GET, 0, 0);
}

// Non-cryptographic bytes from the monotonic clock plus the output address. KUMO has no
// kernel entropy object yet (recorded deviation): HashMap seeds are unguessable-ish, not
// security-grade.
static unsigned long rng_state;
static unsigned long rng_next(void) {
    unsigned long s = rng_state;
    if (s == 0) {
        s = clock_now() ^ (unsigned long)&rng_state ^ 0x9e3779b97f4a7c15UL;
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    rng_state = s;
    return s;
}

extern int *__errno_location(void);

long syscall(long n, ...) {
    __builtin_va_list ap;
    __builtin_va_start(ap, n);
    long result = -ENOSYS;
    if (n == SYS_FUTEX) {
        unsigned int *uaddr = __builtin_va_arg(ap, unsigned int *);
        int op = (int)__builtin_va_arg(ap, int);
        unsigned int val = __builtin_va_arg(ap, unsigned int);
        switch (op & 0x7f) {
        case FUTEX_WAIT:
        case FUTEX_WAIT_BITSET:
            // std only waits when another thread holds the lock; single-threaded paths
            // never park here, and a value mismatch answers EAGAIN exactly as futex(2).
            if (*(volatile unsigned int *)uaddr != val) {
                result = -EAGAIN;
            } else {
                (void)svc2(SYSCALL_FUTEX_WAIT, (long)uaddr, (long)val);
                result = 0;
            }
            break;
        case FUTEX_WAKE:
        case FUTEX_WAKE_BITSET:
            result = svc2(SYSCALL_FUTEX_WAKE, (long)uaddr, (long)val);
            break;
        default:
            break;
        }
    } else if (n == SYS_GETRANDOM) {
        unsigned char *buf = __builtin_va_arg(ap, unsigned char *);
        usize len = __builtin_va_arg(ap, usize);
        for (usize i = 0; i < len; ++i) {
            if (i % 8 == 0) {
                rng_state = rng_next();
            }
            buf[i] = (unsigned char)(rng_state >> ((i % 8) * 8));
        }
        result = (long)len;
    }
    __builtin_va_end(ap);
    if (result == -ENOSYS) {
        // Unknown syscall numbers answer musl-style: errno set, -1 returned. std's
        // copy_file_range/statx-style probes read the error through errno; a raw
        // -ENOSYS return reads as a successful negative result and a stale errno can
        // drive an infinite retry (JOURNAL/869).
        *__errno_location() = ENOSYS;
        return -1;
    }
    return result;
}

// The backtrace printer's unwinder: there is no frame unwinder yet; report end-of-stack.
unsigned long _Unwind_GetIP(void *ctx) {
    (void)ctx;
    return 0;
}

int _Unwind_Backtrace(void *trace, void *arg) {
    (void)trace;
    (void)arg;
    return 5; // _URC_END_OF_STACK
}

// std's random path: the getrandom function (not the syscall()) with /dev/urandom as the
// fallback, which kumo cannot provide. Same documented clock-mixed generator as SYS_getrandom.
// std's TcpListener::accept calls accept4 with SOCK_CLOEXEC. relibc exports accept();
// the flag is vacuous on kumo (no exec), so forward.
struct sockaddr;
typedef unsigned int socklen_t;
extern int accept(int fd, struct sockaddr *addr, socklen_t *addrlen);

int accept4(int fd, struct sockaddr *addr, socklen_t *addrlen, int flags) {
    (void)flags;
    return accept(fd, addr, addrlen);
}

// std's fs::copy probes sendfile/splice before its generic copy loop, and reads
// ENOSYS (or EPERM/EINVAL) as the signal to fall back permanently
// (std/sys/io/kernel_copy/linux.rs). KUMO has neither facility, so both answer ENOSYS
// through relibc's errno and io::copy degrades to the userspace read/write loop
// (JOURNAL/869).
extern int *__errno_location(void);

long sendfile(int out_fd, int in_fd, long *offset, unsigned long count) {
    (void)out_fd;
    (void)in_fd;
    (void)offset;
    (void)count;
    *__errno_location() = ENOSYS;
    return -1;
}

long splice(int fd_in, long *off_in, int fd_out, long *off_out, unsigned long len,
            unsigned int flags) {
    (void)fd_in;
    (void)off_in;
    (void)fd_out;
    (void)off_out;
    (void)len;
    (void)flags;
    *__errno_location() = ENOSYS;
    return -1;
}

// std's Command wait path pulls PidFd::waitid in as a hard symbol; relibc exports
// waitpid only. ENOSYS through errno makes std report the pidfd path unsupported and
// fall back to its waitpid loop (SH4).
int waitid(int idtype, unsigned int id, void *infop, int options) {
    (void)idtype;
    (void)id;
    (void)infop;
    (void)options;
    *__errno_location() = ENOSYS;
    return -1;
}

// std references the glibc-only posix_spawn_file_actions_addchdir_np extension as a hard
// symbol; relibc exports the POSIX addchdir. Forward, so a Command::current_dir request
// records the action (the overlay's spawn refuses Chdir explicitly) instead of failing in
// the shim (SH4).
extern int posix_spawn_file_actions_addchdir(void *actions, const char *path);

int posix_spawn_file_actions_addchdir_np(void *actions, const char *path) {
    return posix_spawn_file_actions_addchdir(actions, path);
}

// std names every spawned thread through the glibc pthread_setname_np, a hard symbol
// relibc does not export. KUMO threads carry no name yet: refuse by name (the pthread
// convention returns the error number), which std ignores (SH5).
int pthread_setname_np(unsigned long thread, const char *name) {
    (void)thread;
    (void)name;
    return ENOSYS;
}

extern char *strerror(int errnum);

int __xpg_strerror_r(int errnum, char *buf, usize buflen) {
    const char *m = strerror(errnum);
    usize n = 0;
    if (m) {
        while (m[n]) {
            n++;
        }
    }
    if (buflen == 0) {
        return 34; // ERANGE
    }
    usize take = n < buflen - 1 ? n : buflen - 1;
    for (usize i = 0; i < take; ++i) {
        buf[i] = m ? m[i] : 0;
    }
    buf[take] = 0;
    return take == n ? 0 : 34;
}
