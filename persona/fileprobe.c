/* SH7-d2 (PLAN/022): the Linux file surface on KUMO, over a vfs session. One line per failed
 * check, then a verdict. /etc/fileprobe.txt is 1000 bytes the image builder writes:
 * byte i is '\n' when i % 50 == 49, else 'a' + (i * 7) % 26. — TEAL */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int failures;

static void check(int ok, const char *what)
{
    if (!ok) {
        printf("fileprobe: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

static char expect(long i)
{
    return i % 50 == 49 ? '\n' : (char)('a' + (i * 7) % 26);
}

static int matches(const char *buf, long from, long len)
{
    for (long i = 0; i < len; i++)
        if (buf[i] != expect(from + i))
            return 0;
    return 1;
}

int main(void)
{
    static char buf[1200];
    struct stat st;

    int fd = open("/etc/fileprobe.txt", O_RDONLY | O_CLOEXEC);
    check(fd >= 3, "open");
    check(fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 1000, "fstat");
    /* read in uneven pieces across the 256-byte vfs reply size */
    long got = 0;
    for (;;) {
        ssize_t n = read(fd, buf + got, 300);
        if (n <= 0)
            break;
        got += n;
    }
    check(got == 1000 && matches(buf, 0, 1000), "read whole file");
    check(read(fd, buf, 10) == 0, "read at EOF is 0");
    check(lseek(fd, 500, SEEK_SET) == 500, "lseek SEEK_SET");
    check(read(fd, buf, 10) == 10 && matches(buf, 500, 10), "read after seek");
    check(lseek(fd, 0, SEEK_CUR) == 510, "lseek SEEK_CUR");
    check(lseek(fd, -10, SEEK_END) == 990, "lseek SEEK_END");
    check(pread(fd, buf, 20, 100) == 20 && matches(buf, 100, 20), "pread");
    check(lseek(fd, 0, SEEK_CUR) == 990, "pread leaves the offset");
    check(close(fd) == 0, "close");
    errno = 0;
    check(close(fd) == -1 && errno == EBADF, "close twice is EBADF");
    errno = 0;
    check(read(fd, buf, 1) == -1 && errno == EBADF, "read a closed fd is EBADF");

    check(stat("/etc/fileprobe.txt", &st) == 0 && st.st_size == 1000, "stat file");
    check(stat("/etc", &st) == 0 && S_ISDIR(st.st_mode), "stat dir");
    errno = 0;
    check(stat("/no/such/file", &st) == -1 && errno == ENOENT, "stat missing is ENOENT");
    errno = 0;
    check(open("/no-such-file", O_RDONLY) == -1 && errno == ENOENT, "open missing is ENOENT");
    errno = 0;
    /* SH7-e1 (j918): the image is read-write for `linux` (cut W1); a directory still is not. */
    check(open("/etc", O_WRONLY) == -1 && errno == EISDIR, "write open of a dir is EISDIR");

    /* relative to the cwd (/) and to a directory fd */
    fd = openat(AT_FDCWD, "etc/fileprobe.txt", O_RDONLY);
    check(fd >= 0 && read(fd, buf, 5) == 5 && matches(buf, 0, 5), "openat AT_FDCWD relative");
    if (fd >= 0)
        close(fd);
    int dir = open("/etc", O_RDONLY | O_DIRECTORY);
    check(dir >= 0, "open dir");
    fd = openat(dir, "fileprobe.txt", O_RDONLY);
    check(fd >= 0 && pread(fd, buf, 5, 995) == 5 && matches(buf, 995, 5), "openat dirfd relative");
    if (fd >= 0)
        close(fd);
    if (dir >= 0)
        close(dir);

    /* getdents64 through readdir: long names come back whole */
    DIR *d = opendir("/bin");
    int saw_hello = 0, saw_probe = 0, entries = 0;
    check(d != 0, "opendir");
    if (d) {
        struct dirent *e;
        while ((e = readdir(d)) != 0) {
            entries++;
            if (strcmp(e->d_name, "hello-dyn") == 0)
                saw_hello = 1;
            if (strcmp(e->d_name, "fileprobe") == 0)
                saw_probe = e->d_type == DT_REG;
        }
        closedir(d);
    }
    check(saw_hello && saw_probe && entries >= 2, "readdir long names");

    /* stdio: fopen/fread go through readv */
    FILE *f = fopen("/etc/fileprobe.txt", "r");
    check(f != 0, "fopen");
    if (f) {
        size_t n = fread(buf, 1, sizeof buf, f);
        check(n == 1000 && matches(buf, 0, 1000), "fread");
        fclose(f);
    }

    printf("fileprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
