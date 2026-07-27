/* j506 */
/*
 * Header-free on purpose: I-a1 proves relibc's runtime boundary, not its
 * installed header set. I-a3 closes the public C header/filesystem gate. — KESTREL
 */

typedef __SIZE_TYPE__ size_t;
typedef __INTPTR_TYPE__ ssize_t;

void *malloc(size_t size);
void free(void *pointer);
ssize_t write(int fd, const void *buffer, size_t length);

static size_t string_length(const char *string) {
    size_t length = 0;
    while (string[length] != '\0') {
        ++length;
    }
    return length;
}

static int write_all(int fd, const char *message) {
    const size_t length = string_length(message);
    return write(fd, message, length) == (ssize_t)length;
}

int main(void) {
    enum { ALLOCATION_BYTES = 384 * 1024, PAGE_BYTES = 4096 };
    volatile unsigned char *allocation = malloc(ALLOCATION_BYTES);
    if (allocation == (void *)0) {
        return 10;
    }

    /* Fresh anonymous VMO pages must be zero before the allocator user touches them. */
    for (size_t offset = 0; offset < ALLOCATION_BYTES; offset += PAGE_BYTES) {
        if (allocation[offset] != 0) {
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
    if (!write_all(2, "relibc: stderr channel -> OK\n")) {
        return 13;
    }
    if (!write_all(1, "relibc: write+exit+malloc 384KiB -> OK\n")) {
        return 14;
    }
    return 0;
}
