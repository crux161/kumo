/* j916: SH7-d3c-2, musl pthreads through the in-process persona shim (Linux clone, futex,
 * exit, set_tid_address, clock_gettime, nanosleep). Built against musl's libc.so and run by
 * musl's own loader, as rustc will be. Each section prints one OK line; any failure prints a
 * FAIL line and the program exits 1. The last thread is left parked on a pipe nobody writes:
 * returning from main must still end the process with 0 (exit_group ends every thread). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int failures;
#define CHECK(ok, what)                                                  \
    do {                                                                 \
        if (!(ok)) {                                                     \
            printf("threadprobe: FAIL %s\n", what);                      \
            failures++;                                                  \
        }                                                                \
    } while (0)

static long now_us(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static void *who(void *arg)
{
    long *out = arg;
    out[0] = syscall(SYS_gettid);
    out[1] = getpid();
    return (void *)0x5a;
}

#define ROUNDS 1000
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static int turn, rounds;

static void *pong(void *arg)
{
    (void)arg;
    pthread_mutex_lock(&m);
    for (int i = 0; i < ROUNDS; i++) {
        while (turn != 1)
            pthread_cond_wait(&c, &m);
        rounds++;
        turn = 0;
        pthread_cond_signal(&c);
    }
    pthread_mutex_unlock(&m);
    return 0;
}

static long counter;
static void *add(void *arg)
{
    (void)arg;
    for (int i = 0; i < 10000; i++) {
        pthread_mutex_lock(&m);
        counter++;
        pthread_mutex_unlock(&m);
    }
    return 0;
}

static int p[2];
static void *reader(void *arg)
{
    return (void *)read(p[0], arg, 64);
}

static void *forever(void *arg)
{
    char byte;
    read(*(int *)arg, &byte, 1);
    return 0;
}

/* Waiting by semaphore, never by yield-spinning: a spinning child starves Sora on KUMO. */
static sem_t detached_ran;
static void *detached(void *arg)
{
    (void)arg;
    sem_post(&detached_ran);
    return 0;
}

int main(void)
{
    pthread_t t;
    void *ret = 0;
    long main_tid = syscall(SYS_gettid);
    CHECK(main_tid == getpid(), "the main thread's tid is the pid");

    long ids[2] = {0, 0};
    CHECK(pthread_create(&t, 0, who, ids) == 0, "create");
    CHECK(pthread_join(t, &ret) == 0 && ret == (void *)0x5a, "join returns the value");
    CHECK(ids[0] > 0 && ids[0] != main_tid && ids[1] == getpid(), "own tid, shared pid");
    printf("threadprobe: create and join, tid %s pid OK\n", ids[0] != main_tid ? "!=" : "==");

    CHECK(pthread_create(&t, 0, pong, 0) == 0, "create pong");
    long t0 = now_us();
    pthread_mutex_lock(&m);
    for (int i = 0; i < ROUNDS; i++) {
        turn = 1;
        pthread_cond_signal(&c);
        while (turn != 0)
            pthread_cond_wait(&c, &m);
    }
    pthread_mutex_unlock(&m);
    long spent = now_us() - t0;
    CHECK(pthread_join(t, 0) == 0 && rounds == ROUNDS, "condvar ping-pong");
    printf("threadprobe: %d condvar round trips OK (%ld us)\n", rounds, spent);

    pthread_t four[4];
    for (int i = 0; i < 4; i++)
        CHECK(pthread_create(&four[i], 0, add, 0) == 0, "create adder");
    for (int i = 0; i < 4; i++)
        CHECK(pthread_join(four[i], 0) == 0, "join adder");
    CHECK(counter == 40000, "contended mutex count");
    printf("threadprobe: 4 threads x 10000 locked increments = %ld OK\n", counter);

    char got[64] = {0};
    struct timespec nap = {0, 50 * 1000 * 1000};
    CHECK(pipe(p) == 0, "pipe");
    CHECK(pthread_create(&t, 0, reader, got) == 0, "create reader");
    nanosleep(&nap, 0);
    CHECK(write(p[1], "wake", 4) == 4, "write to the parked reader");
    CHECK(pthread_join(t, &ret) == 0 && (long)ret == 4 && memcmp(got, "wake", 4) == 0,
          "parked reader woken by a sibling's write");
    printf("threadprobe: a parked pipe reader woke on a sibling's write OK\n");

    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
    sem_init(&detached_ran, 0, 0);
    CHECK(pthread_create(&t, &attr, detached, 0) == 0, "create detached");
    CHECK(sem_wait(&detached_ran) == 0, "detached thread ran");

    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_nsec += 20 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000) {
        deadline.tv_sec += 1;
        deadline.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&m);
    int timed = pthread_cond_timedwait(&c, &m, &deadline);
    pthread_mutex_unlock(&m);
    CHECK(timed == ETIMEDOUT, "a timed condvar wait times out");
    printf("threadprobe: detached thread and timed wait OK\n");

    int q[2];
    CHECK(pipe(q) == 0, "second pipe");
    CHECK(pthread_create(&t, 0, forever, &q[0]) == 0, "create the parked thread");
    nanosleep(&nap, 0);
    if (failures == 0)
        printf("threadprobe: OK (0 failures)\n");
    else
        printf("threadprobe: %d failures\n", failures);
    fflush(stdout);
    return failures ? 1 : 0;
}
