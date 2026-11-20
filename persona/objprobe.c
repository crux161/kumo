//j920
/* Hash rustc's output from a second Linux process on the same boot. */
#include <sha2.h>
#include <stdio.h>

int main(int argc, char **argv)
{
    if (argc != 2) return 2;
    FILE *file = fopen(argv[1], "rb");
    if (!file) {
        perror(argv[1]);
        return 1;
    }

    SHA2_CTX hash;
    unsigned char buffer[4096], digest[SHA256_DIGEST_LENGTH];
    size_t size = 0, count;
    SHA256Init(&hash);
    while ((count = fread(buffer, 1, sizeof(buffer), file)) != 0) {
        SHA256Update(&hash, buffer, count);
        size += count;
    }
    int failed = ferror(file);
    if (fclose(file) != 0 || failed) {
        fprintf(stderr, "objprobe: could not read %s\n", argv[1]);
        return 1;
    }
    SHA256Final(digest, &hash);
    printf("objprobe: %s %zu bytes sha256 ", argv[1], size);
    for (size_t i = 0; i < sizeof(digest); ++i) printf("%02x", digest[i]);
    putchar('\n');
    return 0;
}
