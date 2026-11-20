/* j915: SH7-d3c-1, Linux pipes through the in-process persona; no libc required. */
typedef unsigned long usize;
typedef unsigned int u32;

static long call(usize nr, usize a, usize b, usize c)
{
    register usize x0 __asm__("x0") = a;
    register usize x1 __asm__("x1") = b;
    register usize x2 __asm__("x2") = c;
    register usize x8 __asm__("x8") = nr;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x8) : "memory", "cc");
    return (long)x0;
}

#define SAY(s) call(64, 1, (usize)(s), sizeof(s) - 1)
#define CHECK(ok, s) do { if (!(ok)) { SAY("pipeprobe: FAIL " s "\n"); failures++; } } while (0)
#define NONBLOCK 04000
#define CLOEXEC 02000000
static int failures;
static unsigned char source[5003], got[5003];

static void probe(void)
{
    int p[2] = {-7, -9};
    CHECK(call(59, (usize)p, 1, 0) == -22 && p[0] == -7 && p[1] == -9,
          "invalid flags change nothing");
    CHECK(call(59, 0, 0, 0) == -14, "null output");
    CHECK(call(59, (usize)p, NONBLOCK | CLOEXEC, 0) == 0, "create");
    CHECK(p[0] >= 3 && p[1] >= 3 && p[0] != p[1], "distinct descriptors");
    CHECK(call(25, p[0], 1, 0) == 1 && call(25, p[1], 1, 0) == 1,
          "close on exec recorded");
    CHECK(call(25, p[0], 3, 0) == NONBLOCK && call(25, p[1], 3, 0) == (NONBLOCK | 1),
          "access and nonblock flags");
    CHECK(call(25, p[0], 2, 0) == 0 && call(25, p[0], 1, 0) == 0, "set descriptor flags");
    CHECK(call(25, p[0], 4, 0) == 0 && call(25, p[0], 3, 0) == 0, "clear nonblock");
    CHECK(call(25, p[0], 4, NONBLOCK) == 0, "restore nonblock");
    CHECK(call(63, p[0], (usize)got, 1) == -11, "empty nonblock is EAGAIN");
    CHECK(call(63, p[0], 0, 0) == 0 && call(64, p[1], 0, 0) == 0, "zero bytes");
    CHECK(call(63, p[0], (usize)got, 1) == -11, "zero write queues nothing");
    CHECK(call(63, p[1], (usize)got, 1) == -9 && call(64, p[0], (usize)got, 1) == -9,
          "wrong direction");
    CHECK(call(62, p[0], 0, 0) == -29, "pipe is not seekable");
    usize st0[16], st1[16];
    CHECK(call(80, p[0], (usize)st0, 0) == 0 && call(80, p[1], (usize)st1, 0) == 0,
          "fstat");
    CHECK((((u32 *)st0)[4] & 0170000) == 0010000 && st0[1] == st1[1],
          "matching FIFO inode");
    SAY("pipeprobe: flags, errors and empty reads OK\n");

    for (usize i = 0; i < sizeof(source); ++i) source[i] = (unsigned char)(i * 37 + 11);
    CHECK(call(64, p[1], (usize)source, sizeof(source)) == sizeof(source), "write 5003 bytes");
    CHECK(call(57, p[1], 0, 0) == 0, "close writer with queued bytes");
    CHECK(call(63, p[0], (usize)got, 17) == 17, "short read");
    usize done = 17;
    while (done < sizeof(got)) {
        long n = call(63, p[0], (usize)(got + done), sizeof(got) - done);
        if (n <= 0 || (usize)n > sizeof(got) - done) {
            CHECK(0, "lost message tail");
            break;
        }
        done += n;
    }
    int exact = done == sizeof(got);
    for (usize i = 0; i < done; ++i) if (got[i] != source[i]) exact = 0;
    CHECK(exact, "binary bytes in order");
    CHECK(call(63, p[0], (usize)got, 1) == 0, "EOF only after queued bytes drain");
    CHECK(call(57, p[0], 0, 0) == 0 && call(57, p[0], 0, 0) == -9, "close once");
    SAY("pipeprobe: 5003 exact bytes, short-read tails and EOF OK\n");

    CHECK(call(59, (usize)p, 0, 0) == 0, "blocking pipe");
    usize iov[4] = {(usize)source, 240, (usize)(source + 240), 300};
    CHECK(call(66, p[1], (usize)iov, 2) == 540 && call(63, p[0], (usize)got, 540) == 540,
          "writev vectors share one atomic message");
    int gathered = 1;
    for (usize i = 0; i < 540; ++i) if (got[i] != source[i]) gathered = 0;
    CHECK(gathered, "writev exact bytes");
    CHECK(call(64, p[1], (usize)source, 3) == 3 && call(63, p[0], (usize)got, 3) == 3,
          "blocking descriptor reads queued bytes");
    CHECK(call(57, p[0], 0, 0) == 0 && call(64, p[1], (usize)source, 1) == -32,
          "closed reader is EPIPE");
    CHECK(call(57, p[1], 0, 0) == 0, "close broken writer");

    for (int i = 0; i < 128; ++i) {
        CHECK(call(59, (usize)p, 0, 0) == 0, "reuse create");
        CHECK(p[0] == 3 && p[1] == 4, "reuse slots");
        call(57, p[0], 0, 0);
        call(57, p[1], 0, 0);
    }
    int held[30][2];
    for (int i = 0; i < 30; ++i) CHECK(call(59, (usize)held[i], 0, 0) == 0, "fill descriptors");
    p[0] = -7; p[1] = -9;
    CHECK(call(59, (usize)p, 0, 0) == -24 && p[0] == -7 && p[1] == -9,
          "one free slot cannot half-create a pipe");
    call(57, held[29][0], 0, 0);
    held[29][0] = -1;
    CHECK(call(59, (usize)p, 0, 0) == 0 && p[0] == 61 && p[1] == 63,
          "failed allocation preserved the last slot");
    call(57, p[0], 0, 0); call(57, p[1], 0, 0);
    for (int i = 0; i < 30; ++i) {
        if (held[i][0] >= 0) call(57, held[i][0], 0, 0);
        call(57, held[i][1], 0, 0);
    }
    SAY("pipeprobe: EPIPE, 128 reuse cycles and EMFILE rollback OK\n");
}

__attribute__((noreturn)) void _start(void)
{
    probe();
    if (failures == 0) SAY("pipeprobe: OK (0 failures)\n");
    else SAY("pipeprobe: FAILED\n");
    call(94, failures != 0, 0, 0);
    for (;;) __asm__ volatile("brk #0");
}
