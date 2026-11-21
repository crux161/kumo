/* j925 (SH7-g): fork + execve as Rust's Command does it when a pre_exec hook (Cargo's job
 * server) rules out posix_spawn: a close-on-exec SEQPACKET socketpair carries an exec failure
 * back, the child rearranges descriptors and clears FD_CLOEXEC on one it keeps, then execs
 * in place. Also: the child's memory is a copy. Passes on Linux too. - TEAL */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
static volatile int global = 1;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("forkprobe: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

/* After execve: the kept descriptor is open, the close-on-exec one is gone. */
static int child(char **argv) {
    int keep = atoi(argv[2]);
    int gone = atoi(argv[3]);
    char cwd[64];
    if (!getcwd(cwd, sizeof cwd))
        strcpy(cwd, "?");
    int kept = fcntl(keep, F_GETFD) >= 0;
    int closed = fcntl(gone, F_GETFD) < 0 && errno == EBADF;
    const char *env = getenv("FORKPROBE");
    printf("forkchild cwd=%s kept=%d closed=%d env=%s\n", cwd, kept, closed, env ? env : "-");
    fflush(stdout);
    return 5;
}

static void read_all(int fd, char *buf, size_t len) {
    size_t at = 0;
    ssize_t n;
    while (at + 1 < len && (n = read(fd, buf + at, len - 1 - at)) > 0)
        at += n;
    buf[at] = 0;
}

int main(int argc, char **argv) {
    if (argc > 3 && strcmp(argv[1], "child") == 0)
        return child(argv);

    /* 1. A successful exec: the error socket reads end of stream. */
    int sv[2], out[2], keep[2];
    check(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) == 0, "socketpair");
    check(pipe2(out, O_CLOEXEC) == 0 && pipe2(keep, O_CLOEXEC) == 0, "pipes");
    pid_t pid = fork();
    if (pid == 0) {
        close(sv[0]);
        dup2(out[1], 1);
        fcntl(keep[0], F_SETFD, 0); /* what jobserver's pre_exec does */
        chdir("/bin");
        char a[16], b[16];
        snprintf(a, sizeof a, "%d", keep[0]);
        snprintf(b, sizeof b, "%d", keep[1]);
        char *cargv[] = {"forkprobe", "child", a, b, NULL};
        char *cenv[] = {"FORKPROBE=after-exec", NULL};
        execve("/bin/forkprobe", cargv, cenv);
        int e = errno;
        write(sv[1], &e, sizeof e);
        _exit(127);
    }
    check(pid > 0, "fork");
    close(sv[1]);
    close(out[1]);
    int reported = 0;
    ssize_t got = recv(sv[0], &reported, sizeof reported, 0);
    check(got == 0, "exec success is end of stream on the socket");
    char buf[256];
    read_all(out[0], buf, sizeof buf);
    int status = -1;
    check(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 5,
          "exec'd child exit 5");
    check(strcmp(buf, "forkchild cwd=/bin kept=1 closed=1 env=after-exec\n") == 0, "child output");
    if (failures)
        printf("forkprobe: got [%s] status %#x\n", buf, status);
    close(sv[0]);
    close(out[0]);

    /* 2. A failed exec reports its errno through the socket, then the child exits 127. */
    check(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0, sv) == 0, "socketpair 2");
    pid = fork();
    if (pid == 0) {
        close(sv[0]);
        char *nargv[] = {"nope", NULL};
        execve("/bin/no-such-program", nargv, NULL);
        int e = errno;
        write(sv[1], &e, sizeof e);
        _exit(127);
    }
    close(sv[1]);
    got = recv(sv[0], &reported, sizeof reported, 0);
    check(got == sizeof reported && reported == ENOENT, "exec failure reports ENOENT");
    check(waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 127, "failed child exit 127");
    close(sv[0]);

    /* 3. Memory is the child's own. */
    pid = fork();
    if (pid == 0) {
        global = 2;
        _exit(global);
    }
    check(waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 2, "child wrote its copy");
    check(global == 1, "parent copy unchanged");

    printf("forkprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
