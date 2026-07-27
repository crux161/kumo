/* j506 */
/* j507 */
/*
 * Header-free on purpose: I-a2 proves relibc's startup boundary, not its
 * installed header set. I-a3 closes the public C header/filesystem gate.
 * — KESTREL / TIMBERDOODLE
 */

typedef __SIZE_TYPE__ size_t;
typedef __INTPTR_TYPE__ ssize_t;
typedef __UINTPTR_TYPE__ uintptr_t;

/* Opaque on purpose: I-a3 proves the file boundary, not <stdio.h>'s layout. */
typedef struct _kumo_file FILE;

int printf(const char *format, ...);
void *calloc(size_t count, size_t size);
void free(void *pointer);
ssize_t write(int fd, const void *buffer, size_t length);
int *__errno_location(void);
FILE *fopen(const char *path, const char *mode);
size_t fread(void *buffer, size_t size, size_t count, FILE *stream);
int fclose(FILE *stream);

#define errno (*__errno_location())
#define EBADF 9

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

    printf("relibc: startup+printf+argv+tls+calloc+file -> OK\n");
    return 0;
}
