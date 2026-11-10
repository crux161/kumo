// j853
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
