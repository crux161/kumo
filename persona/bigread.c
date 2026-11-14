/* SH7-d3a (PLAN/022): bulk file reads on KUMO. /etc/big.bin is a file the image builder writes
 * (several MiB, not page-sized): byte i is (i >> 9) ^ (i * 7), truncated to 8 bits. The probe
 * reads it whole with read(), in pieces with pread() at awkward offsets, and through mmap(),
 * and checks every byte. One line per failed check, then a verdict. Each phase prints a line,
 * so the kernel log's timestamps time it. — TEAL */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;

static void check(int ok, const char *what)
{
    if (!ok) {
        printf("bigread: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

static unsigned char expect(long i)
{
    return (unsigned char)((i >> 9) ^ (i * 7));
}

static long first_mismatch(const unsigned char *buf, long from, long len)
{
    for (long i = 0; i < len; i++)
        if (buf[i] != expect(from + i))
            return from + i;
    return -1;
}

int main(void)
{
    struct stat st;
    int fd = open("/etc/big.bin", O_RDONLY | O_CLOEXEC);
    check(fd >= 0, "open /etc/big.bin");
    if (fd < 0)
        return 1;
    check(fstat(fd, &st) == 0, "fstat");
    long size = (long)st.st_size;
    printf("bigread: size %ld, read() begins\n", size);

    /* 1. read() front to back, a megabyte at a time. */
    enum { CHUNK = 1 << 20 };
    unsigned char *buf = malloc(CHUNK);
    check(buf != NULL, "malloc");
    if (buf == NULL)
        return 1;
    long at = 0;
    for (;;) {
        ssize_t n = read(fd, buf, CHUNK);
        if (n <= 0) {
            check(n == 0, "read");
            break;
        }
        long bad = first_mismatch(buf, at, n);
        if (bad >= 0) {
            printf("bigread: FAIL read() byte %ld\n", bad);
            failures++;
            break;
        }
        at += n;
    }
    check(at == size, "read() reached the end");
    printf("bigread: read() done, mmap() begins\n");

    /* 2. The whole file through mmap(), and the zero tail of its last page. */
    long page = 4096;
    long span = (size + page - 1) / page * page;
    unsigned char *map = mmap(NULL, span, PROT_READ, MAP_PRIVATE, fd, 0);
    check(map != MAP_FAILED, "mmap whole");
    if (map != MAP_FAILED) {
        long bad = first_mismatch(map, 0, size);
        if (bad >= 0) {
            printf("bigread: FAIL mmap byte %ld\n", bad);
            failures++;
        }
        for (long i = size; i < span; i++)
            if (map[i] != 0) {
                check(0, "mmap tail past EOF is zero");
                break;
            }
        munmap(map, span);
    }
    /* A window from a page-aligned offset that is not a cluster boundary. */
    long off = 0x123000;
    long len = 777777;
    map = mmap(NULL, len, PROT_READ, MAP_PRIVATE, fd, off);
    check(map != MAP_FAILED, "mmap window");
    if (map != MAP_FAILED) {
        check(first_mismatch(map, off, len) < 0, "mmap window bytes");
        munmap(map, len);
    }
    printf("bigread: mmap() done, pread() begins\n");

    /* 3. pread() at odd offsets and lengths, backwards then forwards (cursor reuse and reset). */
    long offsets[] = { size - 1, size - 5000, 3 * 1048576 + 17, 511, 4097, 0, 2 * 1048576 - 3,
                       1048575, size - 1048577 };
    long lengths[] = { 1, 5000, 300000, 2, 70000, 257, 1048576, 9, 1048577 };
    for (unsigned k = 0; k < sizeof offsets / sizeof offsets[0]; k++) {
        long o = offsets[k], l = lengths[k];
        if (l > CHUNK)
            l = CHUNK;
        ssize_t n = pread(fd, buf, l, o);
        long want = size - o < l ? size - o : l;
        if (n != want || first_mismatch(buf, o, n) >= 0) {
            printf("bigread: FAIL pread(%ld, %ld) = %ld\n", o, l, (long)n);
            failures++;
        }
    }
    check(pread(fd, buf, 10, size) == 0, "pread at EOF");
    close(fd);
    printf("bigread: %s (%d failures)\n", failures ? "FAILED" : "OK", failures);
    return failures != 0;
}
