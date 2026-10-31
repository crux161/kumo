/* j738 — real relibc math and seekable memory streams for Mesa. — SAKER */
#include <errno.h>
#include <limits.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void check(int condition, const char *operation)
{
    if (!condition) {
        printf("G7m libc closure FAILED: %s (errno=%d)\n", operation, errno);
        abort();
    }
}

static void stream_checks(void)
{
    char *buffer = NULL;
    size_t size = 99;
    errno = 0;
    check(open_memstream(NULL, &size) == NULL && errno == EINVAL, "null buffer argument");
    errno = 0;
    check(open_memstream(&buffer, NULL) == NULL && errno == EINVAL, "null size argument");
    FILE *stream = open_memstream(&buffer, &size);
    check(stream != NULL, "empty stream create");
    check(fclose(stream) == 0 && size == 0 && buffer != NULL && buffer[0] == 0, "empty close ownership");
    free(buffer);
    stream = open_memstream(&buffer, &size);
    check(stream != NULL, "stream create");
    check(fprintf(stream, "shader:%d\n", 42) == 10 && fflush(stream) == 0, "formatted output and flush");
    check(size == 10 && strcmp(buffer, "shader:42\n") == 0, "published bytes / terminator");
    for (unsigned i = 0; i < 65536; ++i)
        check(fputc('A' + i % 26, stream) != EOF, "grow beyond old buffer");
    check(fflush(stream) == 0 && size == 65546 && buffer[size] == 0, "grown size and terminator");
    for (unsigned i = 0; i < 65536; ++i)
        check(buffer[10 + i] == (char)('A' + i % 26), "grown contents");
    check(ftell(stream) == (long)size, "position");
    check(fseek(stream, 3, SEEK_SET) == 0 && fflush(stream) == 0, "seek backwards and flush");
    check(size == 3 && memcmp(buffer, "sha", 3) == 0 && buffer[3] == 0, "shorter published view");
    check(fseek(stream, 0, SEEK_END) == 0 && fflush(stream) == 0, "restore high-water view");
    check(size == 65546 && memcmp(buffer, "shader:42\n", 10) == 0, "temporary terminator restored");
    check(fseek(stream, 2, SEEK_SET) == 0 && fwrite("X", 1, 1, stream) == 1, "overwrite");
    check(fseek(stream, 16, SEEK_END) == 0 && fwrite("END", 1, 3, stream) == 3, "seek beyond end and write");
    check(fflush(stream) == 0 && size == 65565, "hole size");
    for (size_t i = 65546; i < 65562; ++i)
        check(buffer[i] == 0, "hole zero-fill");
    check(buffer[2] == 'X' && memcmp(buffer + 65562, "END\0", 4) == 0, "overwrite and final bytes");
    errno = 0;
    check(fseek(stream, -1, SEEK_SET) == -1 && errno == EINVAL && ftell(stream) == 65565, "negative seek refusal");
    errno = 0;
    check(fseeko(stream, LLONG_MAX, SEEK_SET) == -1 && errno == EINVAL, "unrepresentable seek refusal");
    errno = 0;
    check(fileno(stream) == -1 && errno == EBADF, "no descriptor minted");
    check(fclose(stream) == 0 && size == 65565 && buffer[size] == 0, "close publication and caller ownership");
    free(buffer);
}

static void math_checks(void)
{
    volatile float x = 0.25f;
    volatile double zero = 0.0;
    volatile float two = 2.0f;
    volatile float three = 3.0f;
    int exponent;
    check(isnan(NAN) && isinf(INFINITY) && isfinite(x), "math header constants / classification");
    check(fabsf(acosf(x) + asinf(x) - (float)M_PI_2) < 0.00001f, "inverse trig exports");
    check(fabsf(atanf(x) - atan2f(x, 1.0f)) < 0.00001f, "atan exports");
    check(sin(zero) == 0.0 && cos(zero) == 1.0 && sinf((float)zero) == 0.0f && cosf((float)zero) == 1.0f, "sin / cos exports");
    check(exp2f(three) == 8.0f && expf((float)zero) == 1.0f, "exp exports");
    check(frexp(12.0 + zero, &exponent) == 0.75 && exponent == 4, "frexp");
    check(frexpf(12.0f + (float)zero, &exponent) == 0.75f && exponent == 4, "frexpf");
    check(ldexp(0.75 + zero, 4) == 12.0 && ldexpf(0.75f + (float)zero, 4) == 12.0f, "ldexp exports");
    check(llrint(2.25 + zero) == 2 && llround(-2.5 + zero) == -3 && llroundf(-2.5f + (float)zero) == -3, "long long rounding");
    check(log(1.0 + zero) == 0.0 && log10(100.0 + zero) == 2.0 && log2f(8.0f + (float)zero) == 3.0f && logf(1.0f + (float)zero) == 0.0f, "log exports");
    check(lrint(2.25 + zero) == 2 && lrintf(2.25f + (float)zero) == 2 && lroundf(-2.5f + (float)zero) == -3, "long rounding");
    check(pow(two, three) == 8.0 && powf(two, three) == 8.0f, "pow exports");
}

int main(void)
{
    puts("G7m libc closure begin");
    stream_checks();
    math_checks();
    puts("G7m libc closure green: memory stream growth, seek, holes, ownership, refusal, math");
    return 0;
}
