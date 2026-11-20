/* SH7-e1 (j918, PLAN/022 SH7-e, cut W1): the Linux write surface on KUMO, over the image's
 * read-write vfs session, through musl. One line per failed check, a phase line per section,
 * then a verdict. Leaves nothing behind (it removes its own tree). — TEAL */
#define _GNU_SOURCE
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
        printf("writeprobe: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

static unsigned char pattern(long i)
{
    return (unsigned char)(i * 131 + 7);
}

static unsigned char data[5003], back[5003];

int main(void)
{
    struct stat st;
    char cwd[64];
    int fd;

    for (long i = 0; i < (long)sizeof data; i++)
        data[i] = pattern(i);

    /* directories */
    check(mkdir("/tmp", 0755) == 0 || errno == EEXIST, "mkdir /tmp");
    check(mkdir("/tmp/Write-Probe", 0755) == 0, "mkdir a long mixed-case name");
    errno = 0;
    check(mkdir("/tmp/Write-Probe", 0755) == -1 && errno == EEXIST, "mkdir twice is EEXIST");
    check(stat("/tmp/Write-Probe", &st) == 0 && S_ISDIR(st.st_mode), "stat the new dir");

    /* create, write across vfs chunks, read back */
    fd = open("/tmp/Write-Probe/data.bin", O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
    check(fd >= 0, "create");
    check(write(fd, data, sizeof data) == (ssize_t)sizeof data, "write 5003 bytes");
    errno = 0;
    check(read(fd, back, 1) == -1 && errno == EBADF, "read on O_WRONLY is EBADF");
    check(fsync(fd) == 0, "fsync");
    check(fchmod(fd, 0600) == 0, "fchmod");
    check(close(fd) == 0, "close the writer");
    fd = open("/tmp/Write-Probe/data.bin", O_RDONLY);
    check(fd >= 0 && fstat(fd, &st) == 0 && st.st_size == (off_t)sizeof data, "size after write");
    memset(back, 0, sizeof back);
    check(read(fd, back, sizeof back) == (ssize_t)sizeof back && !memcmp(back, data, sizeof data),
          "read back 5003 exact bytes");
    errno = 0;
    check(write(fd, data, 1) == -1 && errno == EBADF, "write on O_RDONLY is EBADF");
    close(fd);
    printf("writeprobe: create, 5003-byte write and read-back OK\n");

    /* pwrite/pread, truncate, append, O_TRUNC, O_EXCL */
    fd = open("/tmp/Write-Probe/data.bin", O_RDWR);
    check(fd >= 0 && pwrite(fd, "XYZ", 3, 100) == 3, "pwrite");
    check(pread(fd, back, 3, 100) == 3 && !memcmp(back, "XYZ", 3), "pread what pwrite wrote");
    check(ftruncate(fd, 1000) == 0 && fstat(fd, &st) == 0 && st.st_size == 1000, "ftruncate");
    check(lseek(fd, 0, SEEK_END) == 1000, "seek to the new end");
    close(fd);
    fd = open("/tmp/Write-Probe/data.bin", O_WRONLY | O_APPEND);
    check(fd >= 0 && write(fd, "tail", 4) == 4, "append");
    close(fd);
    check(stat("/tmp/Write-Probe/data.bin", &st) == 0 && st.st_size == 1004, "size after append");
    errno = 0;
    check(open("/tmp/Write-Probe/data.bin", O_WRONLY | O_CREAT | O_EXCL, 0644) == -1 &&
              errno == EEXIST, "O_EXCL on an existing file is EEXIST");
    errno = 0;
    check(open("/tmp/Write-Probe/none", O_WRONLY) == -1 && errno == ENOENT,
          "write open of a missing file without O_CREAT is ENOENT");
    errno = 0;
    check(open("/tmp/Write-Probe", O_WRONLY) == -1 && errno == EISDIR, "a dir opened to write");
    fd = open("/tmp/Write-Probe/data.bin", O_WRONLY | O_TRUNC);
    check(fd >= 0 && fstat(fd, &st) == 0 && st.st_size == 0, "O_TRUNC empties");
    close(fd);
    printf("writeprobe: pwrite, ftruncate, append, O_TRUNC and O_EXCL OK\n");

    /* rename, cwd, relative opens, a dir fd, unlink, rmdir */
    check(rename("/tmp/Write-Probe/data.bin", "/tmp/Write-Probe/renamed.bin") == 0, "rename");
    errno = 0;
    check(access("/tmp/Write-Probe/data.bin", F_OK) == -1 && errno == ENOENT, "old name gone");
    check(access("/tmp/Write-Probe/renamed.bin", F_OK) == 0, "new name there");
    check(getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/"), "getcwd starts at /");
    check(chdir("/tmp/Write-Probe") == 0, "chdir");
    check(getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/tmp/Write-Probe"), "getcwd after chdir");
    fd = open("rel.txt", O_RDWR | O_CREAT, 0644);
    check(fd >= 0 && write(fd, "relative", 8) == 8, "create relative to the cwd");
    close(fd);
    check(stat("/tmp/Write-Probe/rel.txt", &st) == 0 && st.st_size == 8, "it landed in the cwd");
    errno = 0;
    check(chdir("rel.txt") == -1 && errno == ENOTDIR, "chdir to a file is ENOTDIR");
    check(chdir("..") == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/tmp"), "chdir ..");
    int dir = open("/tmp/Write-Probe", O_RDONLY | O_DIRECTORY);
    check(dir >= 0, "open the dir");
    int names = 0;
    DIR *d = opendir("/tmp/Write-Probe");
    if (d) {
        for (struct dirent *e; (e = readdir(d));)
            names += !strcmp(e->d_name, "renamed.bin") || !strcmp(e->d_name, "rel.txt");
        closedir(d);
    }
    check(names == 2, "readdir sees both files");
    check(unlinkat(dir, "renamed.bin", 0) == 0 && unlinkat(dir, "rel.txt", 0) == 0,
          "unlinkat relative to a dir fd");
    errno = 0;
    check(unlinkat(AT_FDCWD, "/tmp", 0) == -1 && errno == EISDIR, "unlink a dir is EISDIR");
    close(dir);
    check(unlinkat(AT_FDCWD, "/tmp/Write-Probe", AT_REMOVEDIR) == 0, "rmdir the emptied dir");
    errno = 0;
    check(stat("/tmp/Write-Probe", &st) == -1 && errno == ENOENT, "the tree is gone");
    check(chdir("/") == 0, "back to /");
    printf("writeprobe: rename, cwd, dir fds, unlink and rmdir OK\n");

    printf("writeprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
