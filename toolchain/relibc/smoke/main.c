/* j506 */
/* j507 */
/* j511 */
/* j512 */
/*
 * Every declaration below now comes from KUMO's own installed headers, generated from
 * relibc by the vendored cbindgen and installed into `target/kumo-sysroot`. Until J511
 * this file hand-declared each prototype, because there were no headers to include.
 * That was the last thing standing between KUMO and an unmodified third-party C
 * program. — TIMBERDOODLE
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <termios.h>
#include <unistd.h>

/*
 * Thread-local storage, resolved through the local-exec model against `tpidr_el0`.
 * Without relibc's TLS activation this would not be addressable at all.
 */
static __thread unsigned tls_counter;

/* A plain static, for contrast: it lives in the image, not in the mapped TLS block. */
static unsigned image_counter;

int main(int argc, char **argv, char **envp) {
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

    printf("relibc: startup+printf+argv+tls+calloc+file+tty+stdin -> OK\n");
    return 0;
}
