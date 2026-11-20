/* j919: SH7-e2 (PLAN/022 SH7-e, cut S3), Linux signals delivered through the in-process persona
 * shim, through musl. One line per failed check, a phase line per section, then a verdict.
 * Passes on Linux too (checked in the pinned Alpine image before it ran on KUMO). — TEAL */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

static int failures;
#define CHECK(ok, what)                                                     \
    do {                                                                    \
        if (!(ok)) {                                                        \
            printf("sigprobe: FAIL %s (errno %d)\n", what, errno);          \
            failures++;                                                     \
        }                                                                   \
    } while (0)

static volatile sig_atomic_t usr1_count, usr2_count, nested_ok;
static volatile int seen_signo, seen_code, seen_pid;
static volatile uintptr_t seen_pc, seen_stack_local;
static volatile double handler_fp;

static void on_usr1(int sig, siginfo_t *info, void *ctx)
{
    ucontext_t *uc = ctx;
    seen_signo = info->si_signo;
    seen_code = info->si_code;
    seen_pid = info->si_pid;
    seen_pc = uc->uc_mcontext.pc;
    volatile int local = 0;
    seen_stack_local = (uintptr_t)&local;
    /* Clobber the FP/SIMD registers the interrupted code may hold. */
    volatile double x = 3.0;
    for (int i = 0; i < 8; i++)
        x = x * 1.5 + 0.25;
    handler_fp = x;
    usr1_count++;
    (void)sig;
}

static void on_usr2(int sig)
{
    (void)sig;
    usr2_count++;
}

static void on_usr1_nesting(int sig)
{
    (void)sig;
    raise(SIGUSR2); /* a second frame on top of this one */
    nested_ok = usr2_count;
}

static void install(int sig, void (*h)(int), int flags)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = h;
    sa.sa_flags = flags;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, 0);
}

static void install_info(int sig, void (*h)(int, siginfo_t *, void *), int flags)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = h;
    sa.sa_flags = SA_SIGINFO | flags;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, 0);
}

/* Every vector register survives a system call (the shim's own SIMD use must not leak), and
 * one a signal handler interrupts (rt_sigreturn restores the FP/SIMD state). */
static int vregs_survive(long nr, long a0, long a1, long a2)
{
    uint64_t in[32], out[32];
    for (int i = 0; i < 32; i++)
        in[i] = 0x0101010101010101ull * (uint64_t)(i + 1);
    __asm__ volatile(
        "ldp d0, d1, [%[i], #0]\n ldp d2, d3, [%[i], #16]\n ldp d4, d5, [%[i], #32]\n"
        "ldp d6, d7, [%[i], #48]\n ldp d16, d17, [%[i], #128]\n ldp d18, d19, [%[i], #144]\n"
        "ldp d20, d21, [%[i], #160]\n ldp d22, d23, [%[i], #176]\n ldp d24, d25, [%[i], #192]\n"
        "ldp d26, d27, [%[i], #208]\n ldp d28, d29, [%[i], #224]\n ldp d30, d31, [%[i], #240]\n"
        "mov x8, %[n]\n mov x0, %[a0]\n mov x1, %[a1]\n mov x2, %[a2]\n svc #0\n"
        "stp d0, d1, [%[o], #0]\n stp d2, d3, [%[o], #16]\n stp d4, d5, [%[o], #32]\n"
        "stp d6, d7, [%[o], #48]\n stp d16, d17, [%[o], #128]\n stp d18, d19, [%[o], #144]\n"
        "stp d20, d21, [%[o], #160]\n stp d22, d23, [%[o], #176]\n stp d24, d25, [%[o], #192]\n"
        "stp d26, d27, [%[o], #208]\n stp d28, d29, [%[o], #224]\n stp d30, d31, [%[o], #240]\n"
        :
        : [i] "r"(in), [o] "r"(out), [n] "r"(nr), [a0] "r"(a0), [a1] "r"(a1), [a2] "r"(a2)
        : "x0", "x1", "x2", "x8", "memory", "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v16", "v17",
          "v18", "v19", "v20", "v21", "v22", "v23", "v24", "v25", "v26", "v27", "v28", "v29",
          "v30", "v31");
    for (int i = 0; i < 32; i++)
        if ((i < 8 || i >= 16) && in[i] != out[i])
            return 0;
    return 1;
}

static int p[2];
static volatile long reader_result;
static volatile int reader_errno;
static sem_t reader_ready;

static void *reader(void *arg)
{
    char buf[8];
    (void)arg;
    sem_post(&reader_ready);
    errno = 0;
    reader_result = read(p[0], buf, sizeof buf);
    reader_errno = errno;
    return 0;
}

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static int go;
static void *cond_waiter(void *arg)
{
    (void)arg;
    pthread_mutex_lock(&m);
    sem_post(&reader_ready);
    while (!go)
        pthread_cond_wait(&c, &m);
    pthread_mutex_unlock(&m);
    return 0;
}

static volatile int sleep_result, sleep_errno;
static void *sleeper(void *arg)
{
    struct timespec ten = {10, 0}, left = {0, 0};
    (void)arg;
    sem_post(&reader_ready);
    errno = 0;
    sleep_result = nanosleep(&ten, &left);
    sleep_errno = errno;
    return 0;
}

static void nap(void)
{
    struct timespec t = {0, 50 * 1000 * 1000};
    nanosleep(&t, 0);
}

int main(void)
{
    CHECK(vregs_survive(SYS_getpid, 0, 0, 0), "vector registers survive getpid");
    /* writev's service path zeroes a 4 KiB record with SIMD stores in the shim. */
    CHECK(vregs_survive(SYS_writev, 1, 0, 0), "vector registers survive writev");

    /* 1. raise: siginfo and ucontext, FP state of the interrupted code. */
    install_info(SIGUSR1, on_usr1, 0);
    volatile double mine = 1.25;
    CHECK(raise(SIGUSR1) == 0, "raise");
    CHECK(usr1_count == 1, "the handler ran");
    CHECK(seen_signo == SIGUSR1 && seen_code == SI_TKILL && seen_pid == getpid(),
          "siginfo carries signo, SI_TKILL and the pid");
    CHECK(seen_pc != 0, "the ucontext holds the interrupted pc");
    CHECK(mine == 1.25 && handler_fp > 80.0, "FP math in the handler");
    CHECK(vregs_survive(SYS_tkill, gettid(), SIGUSR1, 0) && usr1_count == 2,
          "vector registers survive a handler run at a syscall's return");
    printf("sigprobe: raise, siginfo and ucontext OK\n");

    /* 2. Blocked, pending, delivered on unblock; ignored signals are dropped. */
    install(SIGUSR2, on_usr2, 0);
    sigset_t set, pending;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &set, 0) == 0, "block SIGUSR2");
    raise(SIGUSR2);
    CHECK(usr2_count == 0, "a blocked signal waits");
    CHECK(sigpending(&pending) == 0 && sigismember(&pending, SIGUSR2), "sigpending sees it");
    CHECK(sigprocmask(SIG_UNBLOCK, &set, 0) == 0 && usr2_count == 1,
          "unblocking delivers it as the call returns");
    install(SIGUSR2, SIG_IGN, 0);
    raise(SIGUSR2);
    CHECK(usr2_count == 1, "an ignored signal is dropped");
    install(SIGUSR2, on_usr2, 0);
    printf("sigprobe: masks, sigpending and SIG_IGN OK\n");

    /* 3. sigaltstack + SA_ONSTACK; a handler raising a second signal (nested frames). */
    static char alt[64 * 1024];
    stack_t ss = {.ss_sp = alt, .ss_size = sizeof alt, .ss_flags = 0};
    CHECK(sigaltstack(&ss, 0) == 0, "sigaltstack");
    install_info(SIGUSR1, on_usr1, SA_ONSTACK);
    raise(SIGUSR1);
    CHECK(seen_stack_local >= (uintptr_t)alt && seen_stack_local < (uintptr_t)alt + sizeof alt,
          "SA_ONSTACK runs the handler on the alternate stack");
    install(SIGUSR1, on_usr1_nesting, 0);
    usr2_count = 0;
    raise(SIGUSR1);
    CHECK(nested_ok == 1 && usr2_count == 1, "a signal raised inside a handler nests");
    printf("sigprobe: altstack and nested frames OK\n");

    /* 4. Another thread parked in a pipe read: EINTR without SA_RESTART (rustc's jobserver
     * helper), a restart with it. */
    install(SIGUSR2, on_usr2, 0);
    CHECK(pipe(p) == 0, "pipe");
    pthread_t t;
    sem_init(&reader_ready, 0, 0);
    CHECK(pthread_create(&t, 0, reader, 0) == 0, "create reader");
    sem_wait(&reader_ready);
    nap();
    usr2_count = 0;
    CHECK(pthread_kill(t, SIGUSR2) == 0, "pthread_kill");
    pthread_join(t, 0);
    CHECK(usr2_count == 1 && reader_result == -1 && reader_errno == EINTR,
          "a parked read answers EINTR");
    install(SIGUSR2, on_usr2, SA_RESTART);
    CHECK(pthread_create(&t, 0, reader, 0) == 0, "create reader again");
    sem_wait(&reader_ready);
    nap();
    usr2_count = 0;
    pthread_kill(t, SIGUSR2);
    nap();
    CHECK(write(p[1], "data", 4) == 4, "write after the signal");
    pthread_join(t, 0);
    CHECK(usr2_count == 1 && reader_result == 4, "SA_RESTART restarts the read");
    printf("sigprobe: EINTR and SA_RESTART on a parked pipe read OK\n");

    /* 5. A thread parked on a futex (condvar) takes its handler; a sleep answers EINTR. */
    usr2_count = 0;
    CHECK(pthread_create(&t, 0, cond_waiter, 0) == 0, "create cond waiter");
    sem_wait(&reader_ready);
    nap();
    pthread_kill(t, SIGUSR2);
    nap();
    CHECK(usr2_count == 1, "a futex-parked thread runs its handler");
    pthread_mutex_lock(&m);
    go = 1;
    pthread_cond_signal(&c);
    pthread_mutex_unlock(&m);
    pthread_join(t, 0);
    install(SIGUSR2, on_usr2, 0);
    CHECK(pthread_create(&t, 0, sleeper, 0) == 0, "create sleeper");
    sem_wait(&reader_ready);
    nap();
    pthread_kill(t, SIGUSR2);
    pthread_join(t, 0);
    CHECK(sleep_result == -1 && sleep_errno == EINTR, "a 10 s sleep answers EINTR");
    printf("sigprobe: futex-parked handler and interrupted sleep OK\n");

    printf("sigprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
