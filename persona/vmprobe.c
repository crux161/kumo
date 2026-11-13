/* SH7-d1 (PLAN/022): Linux address-space semantics on KUMO's page-range VmarUnmap/VmarProtect,
 * as musl's library loader and std use them. One line per check that fails, then a verdict.
 * Built against musl by scripts/persona-payloads.sh; runs inside persona-linux-shim. — TEAL */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

static int failures;

static void check(int ok, const char *what)
{
    if (!ok) {
        printf("vmprobe: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

int main(void)
{
    const long pg = 4096;
    unsigned char *p = mmap(0, 4 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        puts("vmprobe: FAIL mmap");
        return 1;
    }
    for (int i = 0; i < 4; i++)
        memset(p + i * pg, 'A' + i, pg);

    /* PROT_NONE keeps memory: protecting back brings the same bytes. */
    check(mprotect(p + pg, pg, PROT_NONE) == 0, "mprotect none");
    check(mprotect(p + pg, pg, PROT_READ) == 0, "mprotect read");
    check(p[pg] == 'B' && p[2 * pg - 1] == 'B', "contents survive PROT_NONE");
    /* A middle page can be made read-only and the pages around it stay writable. */
    check(mprotect(p + 2 * pg, pg, PROT_READ) == 0, "mprotect middle");
    p[0] = 'a';
    p[3 * pg] = 'd';
    check(p[0] == 'a' && p[3 * pg] == 'd' && p[2 * pg] == 'C', "neighbours writable");
    /* munmap of one page leaves the rest. */
    check(munmap(p + 2 * pg, pg) == 0, "munmap middle");
    check(p[pg] == 'B' && p[3 * pg] == 'd', "neighbours survive munmap");
    /* mprotect across the hole is ENOMEM and changes nothing. */
    errno = 0;
    check(mprotect(p, 4 * pg, PROT_READ) == -1 && errno == ENOMEM, "mprotect over hole is ENOMEM");
    p[0] = 'z';
    check(p[0] == 'z', "still writable after refused mprotect");
    /* MAP_FIXED replaces: fresh anonymous memory over page 0 reads zero. */
    unsigned char *q = mmap(p, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    check(q == p && p[0] == 0 && p[pg - 1] == 0, "MAP_FIXED replaces");
    check(p[pg] == 'B', "MAP_FIXED leaves its neighbour");
    /* munmap of a range that is already unmapped succeeds (Linux). */
    check(munmap(p + 2 * pg, pg) == 0, "munmap of nothing");
    check(munmap(p, 4 * pg) == 0, "munmap whole span across the hole");

    printf("vmprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
