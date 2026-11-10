/* j512 */
/* j513 */
/* j514 */
/* j535 */
/* j557 */
/*
 * Every declaration below now comes from KUMO's own installed headers, generated from
 * relibc by the vendored cbindgen and installed into `target/kumo-sysroot`. Until J511
 * this file hand-declared each prototype, because there were no headers to include.
 * That was the last thing standing between KUMO and an unmodified third-party C
 * program. — TIMBERDOODLE
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdarg.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

/* A populated screen fixture on Sora's RAM FAT volume, never on USB. It makes
 * Kilo's actual repaint exceed one IPC message at 1600x900. — KESTREL */
static int screen_fixture(void) {
    struct winsize window;
    if (ioctl(STDOUT_FILENO, TIOCGWINSZ, &window) != 0 ||
        window.ws_row < 3 || window.ws_row > 128 || window.ws_col < 20 || window.ws_col > 512) {
        return 50;
    }
    FILE *stream = fopen("/J557.TXT", "w");
    if (!stream) return 51;
    char row[513];
    size_t total = 0;
    for (unsigned y = 0; y < window.ws_row - 2; ++y) {
        memset(row, 'A' + y % 26, window.ws_col);
        char label[24];
        int len = snprintf(label, sizeof(label), "j557 row %02u ", y + 1);
        memcpy(row, label, (size_t)len);
        row[window.ws_col] = '\n';
        size_t n = window.ws_col + 1;
        if (fwrite(row, 1, n, stream) != n) return 52;
        total += n;
    }
    if (fclose(stream) != 0) return 53;
    printf("relibc: screen fixture /J557.TXT %ux%u %zu bytes\n",
           window.ws_col, window.ws_row, total);
    return 0;
}

/*
 * Thread-local storage, resolved through the local-exec model against `tpidr_el0`.
 * Without relibc's TLS activation this would not be addressable at all.
 */
static __thread unsigned tls_counter;

/* A plain static, for contrast: it lives in the image, not in the mapped TLS block. */
static unsigned image_counter;


/* ---------------------------------------------------------------------------------------
 * NET13a: the C socket surface over KUMO's netstack — the first UDP subset.
 *
 * The launcher grant decides whether sockets exist at all (`net relibc-smoke`), so the
 * negative case is a first-class mode: without the grant every socket() call must fail
 * with EACCES, never pretend to succeed. With the grant, this block exercises exactly the
 * NET13a bar: socket/bind/sendto/recvfrom/close over loopback with exact bytes and source
 * addresses, sockaddr validation, errno mapping, descriptor exhaustion, and then the NET9
 * real-peer exchange under scripts/net9-peer.py's unchanged payload contract.
 * --------------------------------------------------------------------------------------- */

/*
 * The serial witness. On the metal the console stream belongs to the framebuffer console,
 * so a printf here is watched on HDMI but not captured on the PL011; the kernel-log ring
 * is the serial-visible record (`LogWrite`, NOTICE level). Every acceptance line goes to
 * both, formatted once.
 */
static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    va_list ap2;
    va_copy(ap2, ap);
    vprintf(fmt, ap);
    fflush(stdout);
    char line[192];
    const int n = vsnprintf(line, sizeof(line), fmt, ap2);
    va_end(ap2);
    va_end(ap);
    if (n <= 0) return;
    size_t len = ((size_t)n < sizeof(line)) ? (size_t)n : sizeof(line) - 1;
    if (len < sizeof(line)) line[len++] = '\n';
    register long x8 __asm__("x8") = 47; /* LogWrite */
    register long x0 __asm__("x0") = 5;  /* NOTICE */
    register long x1 __asm__("x1") = (long)(const void *)line;
    register long x2 __asm__("x2") = (long)len;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
}

static int udp_fail(const char *what) {
    say("relibc: udp FAIL %s errno=%d\n", what, errno);
    return 60;
}

static int udp_nogrant(void) {
    errno = 0;
    const int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd != -1 || errno != EACCES) {
        say("relibc: udp-nogrant FAIL fd=%d errno=%d\n", fd, errno);
        return 61;
    }
    say("relibc: udp-nogrant socket denied errno=%d -> OK\n", errno);
    return 0;
}

static int udp_loopback(void) {
    /* Unusable requests fail by name before any capability is consumed. */
    errno = 0;
    if (socket(AF_INET6, SOCK_DGRAM, 0) != -1 || errno != EAFNOSUPPORT) {
        return udp_fail("socket AF_INET6");
    }
    errno = 0;
    if (socket(AF_INET, SOCK_STREAM, 0) != -1 || errno != EPROTONOSUPPORT) {
        return udp_fail("socket SOCK_STREAM");
    }
    errno = 0;
    if (socket(AF_INET, SOCK_DGRAM, 132) != -1 || errno != EPROTONOSUPPORT) {
        return udp_fail("socket protocol 132");
    }

    const int a = socket(AF_INET, SOCK_DGRAM, 0);
    const int b = socket(AF_INET, SOCK_DGRAM, 0);
    if (a < 3 || b < 3 || a == b) return udp_fail("socket udp");
    say("relibc: udp sockets a=%d b=%d\n", a, b);

    struct sockaddr_in addr_a;
    struct sockaddr_in addr_b;
    memset(&addr_a, 0, sizeof(addr_a));
    memset(&addr_b, 0, sizeof(addr_b));
    addr_a.sin_family = AF_INET;
    addr_a.sin_port = htons(7801);
    addr_a.sin_addr.s_addr = htonl(0x7f000001u);
    addr_b.sin_family = AF_INET;
    addr_b.sin_port = htons(7802);
    addr_b.sin_addr.s_addr = htonl(0x7f000001u);
    if (bind(a, (const struct sockaddr *)&addr_a, sizeof(addr_a)) != 0) return udp_fail("bind a");
    if (bind(b, (const struct sockaddr *)&addr_b, sizeof(addr_b)) != 0) return udp_fail("bind b");
    /* A short sockaddr is invalid, and a second claim of the same address conflicts. */
    errno = 0;
    if (bind(a, (const struct sockaddr *)&addr_a, 8) != -1 || errno != EINVAL) {
        return udp_fail("bind short sockaddr");
    }
    const int clash = socket(AF_INET, SOCK_DGRAM, 0);
    if (clash < 0) return udp_fail("socket clash");
    errno = 0;
    if (bind(clash, (const struct sockaddr *)&addr_a, sizeof(addr_a)) != -1 ||
        errno != EADDRINUSE) {
        return udp_fail("bind clash");
    }
    if (close(clash) != 0) return udp_fail("close clash");

    /* Exact bytes both ways, with the source address the stack reports. */
    static const char ping[] = "kumo-c-udp";
    if (sendto(a, ping, sizeof(ping) - 1, 0, (const struct sockaddr *)&addr_b,
               sizeof(addr_b)) != (ssize_t)(sizeof(ping) - 1)) {
        return udp_fail("sendto a->b");
    }
    {
        char got[32];
        struct sockaddr_in from;
        socklen_t from_len = sizeof(from);
        const ssize_t got_len =
            recvfrom(b, got, sizeof(got), 0, (struct sockaddr *)&from, &from_len);
        if (got_len != (ssize_t)(sizeof(ping) - 1) ||
            memcmp(got, ping, sizeof(ping) - 1) != 0) {
            return udp_fail("recvfrom b");
        }
        if (from_len != sizeof(from) || from.sin_family != AF_INET ||
            ntohs(from.sin_port) != 7801 || from.sin_addr.s_addr != htonl(0x7f000001u)) {
            return udp_fail("recvfrom source");
        }
        say("relibc: udp datagram %zd B from 127.0.0.1:%u exact\n", got_len,
               (unsigned)ntohs(from.sin_port));
    }

    /* The zero-length and maximum datagrams are legal and exact. */
    if (sendto(b, "", 0, 0, (const struct sockaddr *)&addr_a, sizeof(addr_a)) != 0) {
        return udp_fail("sendto zero");
    }
    {
        char got[8];
        if (recvfrom(a, got, sizeof(got), 0, (struct sockaddr *)0, (socklen_t *)0) != 0) {
            return udp_fail("recvfrom zero");
        }
    }
    static char big[1472];
    memset(big, 'm', sizeof(big));
    if (sendto(a, big, sizeof(big), 0, (const struct sockaddr *)&addr_b, sizeof(addr_b)) !=
        (ssize_t)sizeof(big)) {
        return udp_fail("sendto max");
    }
    {
        static char echo[1472];
        const ssize_t got_len =
            recvfrom(b, echo, sizeof(echo), 0, (struct sockaddr *)0, (socklen_t *)0);
        if (got_len != (ssize_t)sizeof(echo) || memcmp(echo, big, sizeof(big)) != 0) {
            return udp_fail("recvfrom max");
        }
    }
    say("relibc: udp zero-length and 1472-byte datagrams exact\n");

    /* Nothing waiting: a nonblocking receive says so without blocking. */
    errno = 0;
    if (recvfrom(b, big, sizeof(big), MSG_DONTWAIT, (struct sockaddr *)0, (socklen_t *)0) != -1 ||
        errno != EAGAIN) {
        return udp_fail("recvfrom nonblocking");
    }

    /* The descriptor table holds 16; stdio holds three, so the table runs out first. */
    int extra[16];
    int held = 0;
    int last = 0;
    errno = 0;
    while (held < (int)(sizeof(extra) / sizeof(extra[0]))) {
        last = socket(AF_INET, SOCK_DGRAM, 0);
        if (last < 0) break;
        extra[held++] = last;
    }
    if (last != -1 || errno != EMFILE) return udp_fail("descriptor exhaustion");
    say("relibc: udp descriptor exhaustion after %d sockets errno=%d\n", held, errno);
    for (int i = 0; i < held; ++i) {
        if (close(extra[i]) != 0) return udp_fail("close exhausted");
    }

    /* close frees the descriptor, and a closed socket refuses further use. */
    if (close(a) != 0) return udp_fail("close a");
    errno = 0;
    if (close(a) != -1 || errno != EBADF) return udp_fail("close twice");
    errno = 0;
    if (recvfrom(a, big, sizeof(big), MSG_DONTWAIT, (struct sockaddr *)0, (socklen_t *)0) != -1 ||
        errno != EBADF) {
        return udp_fail("recvfrom closed");
    }
    if (close(b) != 0) return udp_fail("close b");

    say("relibc: udp loopback+edges -> OK\n");
    return 0;
}

/* NET9's payload contract, so scripts/net9-peer.py reconciles both directions unchanged:
 * index 0 is the zero-length datagram, index 1 the 1472-byte maximum, every other index
 * the prefix plus index % 16 fill bytes. */
static size_t wire_build_payload(char *out, const char *side, int index) {
    if (index == 0) return 0;
    size_t at = 0;
    const char *p;
    for (p = side; *p; ++p) out[at++] = *p;
    for (p = "-net9-"; *p; ++p) out[at++] = *p;
    char digits[8];
    int n = 0;
    int value = index;
    while (value > 0) {
        digits[n++] = (char)('0' + value % 10);
        value /= 10;
    }
    while (n > 0) out[at++] = digits[--n];
    out[at++] = '-';
    if (index == 1) {
        while (at < 1472) out[at++] = 0;
    } else {
        for (int k = 0; k < index % 16; ++k) out[at++] = 'x';
    }
    return at;
}

static void wire_pause_ms(long ms) {
    struct timespec start;
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        const long elapsed =
            (now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000;
        if (elapsed >= ms) break;
    }
}

static int udp_wire(const char *ip_text, const char *port_text) {
    struct in_addr peer_addr;
    if (inet_aton(ip_text, &peer_addr) != 1) {
        say("relibc: udp wire FAIL address\n");
        return 62;
    }
    const int peer_port = atoi(port_text);
    if (peer_port <= 0 || peer_port > 65535) {
        say("relibc: udp wire FAIL port\n");
        return 62;
    }
    const int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0) return udp_fail("wire socket");
    struct sockaddr_in local;
    memset(&local, 0, sizeof(local));
    local.sin_family = AF_INET;
    if (bind(s, (const struct sockaddr *)&local, sizeof(local)) != 0) {
        return udp_fail("wire bind");
    }
    struct sockaddr_in peer;
    memset(&peer, 0, sizeof(peer));
    peer.sin_family = AF_INET;
    peer.sin_port = htons((unsigned short)peer_port);
    peer.sin_addr = peer_addr;

    static char tx[1472];
    for (int i = 0; i < 24; ++i) {
        const size_t len = wire_build_payload(tx, "kumo", i);
        int attempts = 0;
        for (;;) {
            if (sendto(s, tx, len, 0, (const struct sockaddr *)&peer, sizeof(peer)) ==
                (ssize_t)len) {
                break;
            }
            /* The wire may not be up yet (autoexec runs before the NIC finishes), and
             * the first moments after wire-up are transient: the frame provider's TX
             * credits flow a beat after the interface appears, so a send can still
             * answer EAGAIN. Both are retried under one bounded budget. */
            if ((errno != ENETUNREACH && errno != EAGAIN) || ++attempts > 1500) {
                return udp_fail("wire send");
            }
            wire_pause_ms(200);
        }
    }
    say("relibc: udp wire 24 datagrams sent\n");
    static char rx[1472];
    static char want[1472];
    for (int i = 0; i < 24; ++i) {
        struct sockaddr_in from;
        socklen_t from_len = sizeof(from);
        const ssize_t got = recvfrom(s, rx, sizeof(rx), 0, (struct sockaddr *)&from, &from_len);
        if (got < 0) return udp_fail("wire recv");
        const size_t len = wire_build_payload(want, "peer", i);
        if ((size_t)got != len || memcmp(rx, want, len) != 0) {
            say("relibc: udp wire FAIL peer datagram %d len=%zd want=%zu\n", i, got, len);
            return 63;
        }
        if (sendto(s, rx, (size_t)got, 0, (const struct sockaddr *)&from, from_len) != got) {
            return udp_fail("wire echo");
        }
    }
    say("relibc: udp wire 24 sent + 24 peer datagrams echoed exact -> OK\n");
    if (close(s) != 0) return udp_fail("wire close");
    return 0;
}

int main(int argc, char **argv, char **envp) {
    if (argc > 1 && strcmp(argv[1], "screen") == 0) return screen_fixture();
    if (argc > 1 && strcmp(argv[1], "udp") == 0) return udp_loopback();
    if (argc > 1 && strcmp(argv[1], "udp-nogrant") == 0) return udp_nogrant();
    if (argc > 3 && strcmp(argv[1], "wire") == 0) return udp_wire(argv[2], argv[3]);
    enum { ALLOCATION_BYTES = 384 * 1024, PAGE_BYTES = 4096 };

    /* argv reached C through KUMO's argv VMO, not through an inherited POSIX stack. */
    printf("relibc: argc=%d\n", argc);
    for (int index = 0; index < argc; ++index) {
        printf("relibc: argv[%d]=%s\n", index, argv[index]);
    }

    /*
     * KUMO grants no environment capability yet, so the startup view carries an empty
     * environment. Check that, rather than let a stray pointer read as inherited state.
     */
    if (envp == (char **)0 || envp[0] != (char *)0) {
        return 20;
    }

    /* TLS round-trip: fresh storage starts zero, then holds a value derived at runtime. */
    if (tls_counter != 0) {
        return 21;
    }
    tls_counter = (unsigned)argc * 7u + 3u;
    image_counter = tls_counter;
    if (tls_counter != image_counter || (uintptr_t)&tls_counter == (uintptr_t)&image_counter) {
        return 22;
    }

    /*
     * relibc's own `errno` is thread-local, so a failing libc call proves the same
     * activation from inside the library. fd 9 was never granted: the write must be
     * refused rather than reach some other capability.
     */
    errno = 0;
    if (write(9, "", 1) != -1 || errno != EBADF) {
        return 23;
    }
    printf("relibc: tls=%u errno=%d tls@%p image@%p\n", tls_counter, errno,
           (void *)&tls_counter, (void *)&image_counter);

    /*
     * `calloc`, not `malloc`: by this point relibc's own startup and stdio have already
     * used the arena, so a `malloc` span may legitimately carry allocator bookkeeping.
     * `calloc` is the allocation whose zeroing C actually guarantees — and dlmalloc keeps
     * that guarantee cheaply here only because it is told KUMO's anonymous VMOs arrive
     * zeroed, so a dirty VMO would surface as a non-zero byte below.
     */
    volatile unsigned char *allocation = calloc(ALLOCATION_BYTES, 1);
    if (allocation == (void *)0) {
        return 10;
    }
    for (size_t offset = 0; offset < ALLOCATION_BYTES; offset += PAGE_BYTES) {
        if (allocation[offset] != 0) {
            printf("relibc: nonzero at +%zu = 0x%02x base=%p\n", offset,
                   (unsigned)allocation[offset], (void *)allocation);
            return 11;
        }
    }

    /* Touch the entire span, not just its first page, so record-only maps cannot pass. */
    for (size_t offset = 0; offset < ALLOCATION_BYTES; ++offset) {
        allocation[offset] = (unsigned char)((offset >> 12) ^ 0x5a);
    }
    for (size_t offset = 0; offset < ALLOCATION_BYTES; offset += PAGE_BYTES) {
        if (allocation[offset] != (unsigned char)((offset >> 12) ^ 0x5a)) {
            return 12;
        }
    }
    free((void *)allocation);

    /* Direct `write` still works beside buffered stdio; both land on the one channel. */
    if (write(2, "relibc: stderr channel -> OK\n", 29) != 29) {
        return 13;
    }

    /*
     * I-a3: a real file, opened by name through the granted filesystem capability and
     * read by stdio. Nothing here knows the file is FAT, is served by another process, or
     * arrives over a channel — which is the point of the gate.
     */
    FILE *stream = fopen("/HELLO.TXT", "r");
    if (stream == (FILE *)0) {
        printf("relibc: fopen failed errno=%d\n", errno);
        return 30;
    }
    char contents[32];
    const size_t got = fread(contents, 1, sizeof(contents) - 1, stream);
    contents[got] = '\0';
    if (fclose(stream) != 0) {
        return 31;
    }
    if (got == 0) {
        return 32;
    }
    printf("relibc: fopen+fread /HELLO.TXT (%zu bytes) -> %s\n", got, contents);

    /* A path the volume does not have must fail, not resolve to something else. */
    errno = 0;
    if (fopen("/NOSUCH.TXT", "r") != (FILE *)0) {
        return 33;
    }
    printf("relibc: absent path refused errno=%d\n", errno);

    /*
     * J535: a relative name resolves against the root of the granted filesystem, so a
     * missing one is ENOENT - the answer kilo's `kilo new.txt` relies on to start an empty
     * buffer - and an existing one opens the same file as its absolute spelling.
     */
    stream = fopen("./HELLO.TXT", "r");
    if (stream == (FILE *)0) {
        printf("relibc: relative fopen failed errno=%d\n", errno);
        return 46;
    }
    char relative[32];
    const size_t relative_got = fread(relative, 1, sizeof(relative) - 1, stream);
    relative[relative_got] = '\0';
    if (fclose(stream) != 0) {
        return 47;
    }
    if (relative_got != got || strcmp(relative, contents) != 0) {
        return 48;
    }
    errno = 0;
    if (fopen("NOSUCH.TXT", "r") != (FILE *)0 || errno != ENOENT) {
        printf("relibc: relative absent path errno=%d\n", errno);
        return 49;
    }
    printf("relibc: relative ./HELLO.TXT -> %s, relative absent errno=%d\n", relative, errno);

    static const char saved[] = "clouds over kumo\n";

    if (argc > 1 && strcmp(argv[1], "persisted") == 0) {
        stream = fopen("/KILO.TXT", "r");
        if (stream == (FILE *)0) {
            printf("relibc: persisted file missing errno=%d\n", errno);
            return 44;
        }
        char prior[32];
        const size_t prior_got = fread(prior, 1, sizeof(prior), stream);
        if (fclose(stream) != 0 || prior_got != sizeof(saved) - 1 ||
            memcmp(prior, saved, sizeof(saved) - 1) != 0) {
            return 45;
        }
        printf("relibc: KILO.TXT survived fatfs replacement -> OK\n");
    }

    /* Kilo's complete save sequence: create/open read-write, resize, one write, close. */
    int saved_fd = open("/KILO.TXT", O_RDWR | O_CREAT, 0644);
    if (saved_fd < 0 || ftruncate(saved_fd, sizeof(saved) - 1) != 0 ||
        write(saved_fd, saved, sizeof(saved) - 1) != (ssize_t)(sizeof(saved) - 1) ||
        fsync(saved_fd) != 0 || close(saved_fd) != 0) {
        printf("relibc: kilo save sequence failed errno=%d\n", errno);
        return 41;
    }
    stream = fopen("/KILO.TXT", "r");
    if (stream == (FILE *)0) {
        return 42;
    }
    char saved_back[sizeof(saved)];
    const size_t saved_got = fread(saved_back, 1, sizeof(saved_back) - 1, stream);
    saved_back[saved_got] = '\0';
    if (fclose(stream) != 0 || saved_got != sizeof(saved) - 1 ||
        memcmp(saved_back, saved, sizeof(saved) - 1) != 0) {
        return 43;
    }
    printf("relibc: kilo create+truncate+write+flush+reopen -> OK\n");

    /*
     * `ttyd`, not libc, owns echo and line editing. Enter raw mode through its
     * explicitly granted control channel, read the state back, then restore it before
     * continuing. This is the exact tcgetattr/cfmakeraw/tcsetattr sequence kilo uses.
     */
    struct termios cooked;
    if (tcgetattr(STDIN_FILENO, &cooked) != 0 ||
        (cooked.c_lflag & (ICANON | ECHO)) != (ICANON | ECHO)) {
        printf("relibc: tcgetattr cooked failed errno=%d\n", errno);
        return 34;
    }
    struct termios raw = cooked;
    cfmakeraw(&raw);
    if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) != 0) {
        printf("relibc: tcsetattr raw failed errno=%d\n", errno);
        return 35;
    }
    struct termios observed;
    if (tcgetattr(STDIN_FILENO, &observed) != 0 ||
        (observed.c_lflag & (ICANON | ECHO)) != 0) {
        return 36;
    }
    if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &cooked) != 0) {
        return 37;
    }
    printf("relibc: tty cooked -> raw -> cooked via ttyd -> OK\n");

    struct winsize window;
    if (ioctl(STDOUT_FILENO, TIOCGWINSZ, &window) != 0 || window.ws_row == 0 ||
        window.ws_col == 0) {
        printf("relibc: TIOCGWINSZ failed errno=%d\n", errno);
        return 38;
    }
    printf("relibc: tty window %ux%u via ttyd -> OK\n", window.ws_col,
           window.ws_row);

    /*
     * Standard input, read the way an editor reads it: raw bytes off the descriptor, not
     * a line through stdio. A granted channel delivers one message per read and answers
     * zero once its writer is gone, which is exactly end of file.
     */
    char line[64];
    const ssize_t got_in = read(0, line, sizeof(line) - 1);
    if (got_in < 0) {
        printf("relibc: stdin read failed errno=%d\n", errno);
        return 40;
    }
    line[got_in] = '\0';
    /* Trim one trailing newline so the echo stays on one line. */
    if (got_in > 0 && line[got_in - 1] == '\n') {
        line[got_in - 1] = '\0';
    }
    if (read(0, line + got_in, 1) != 0) {
        return 41; /* the writer was closed before launch: the next read must be EOF */
    }
    printf("relibc: stdin read %zd bytes -> \"%s\" then EOF\n", got_in, line);

    printf("relibc: startup+printf+argv+tls+calloc+file+write+tty+winsize+stdin -> OK\n");
    return 0;
}
