/* j924 (SH7-f): process spawn as musl's posix_spawn does it (CLONE_VM | CLONE_VFORK, file
 * actions in the child, execve, waitpid) - what rustc's linker launch and Cargo's rustc
 * launches need. The probe spawns itself as the child. Passes on Linux too. - TEAL */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("spawnprobe: FAIL %s (errno %d)\n", what, errno);
        failures++;
    }
}

/* The child: report what it was given, then exit with the status its first argument names. */
static int child(int argc, char **argv) {
    char buf[64];
    char cwd[128];
    ssize_t in = read(0, buf, sizeof buf);
    const char *probe = getenv("SPAWNPROBE");
    int fd = argc > 3 ? atoi(argv[3]) : -1;
    char head[4] = {0};
    ssize_t got = fd >= 0 ? read(fd, head, 3) : -1;
    if (!getcwd(cwd, sizeof cwd))
        strcpy(cwd, "?");
    printf("child argc=%d arg=%s env=%s stdin=%zd cwd=%s fd=%s\n", argc, argv[2],
           probe ? probe : "-", in, cwd, got == 3 ? head : "-");
    fprintf(stderr, "child stderr line\n");
    fflush(stdout);
    return atoi(argv[2]);
}

/* Read both pipes to end of stream as Rust's read2 does: poll, then read what is ready. */
static void drain(int out, int err, char *obuf, size_t olen, char *ebuf, size_t elen) {
    size_t on = 0, en = 0;
    struct pollfd fds[2] = {{out, POLLIN, 0}, {err, POLLIN, 0}};
    int open = 2;
    while (open > 0) {
        if (poll(fds, 2, -1) < 0) {
            check(0, "poll");
            return;
        }
        for (int i = 0; i < 2; i++) {
            if (fds[i].fd < 0 || fds[i].revents == 0)
                continue;
            char *buf = i == 0 ? obuf + on : ebuf + en;
            size_t room = i == 0 ? olen - 1 - on : elen - 1 - en;
            ssize_t n = read(fds[i].fd, buf, room);
            if (n <= 0) {
                fds[i].fd = -1;
                open--;
            } else if (i == 0) {
                on += n;
            } else {
                en += n;
            }
        }
    }
    obuf[on] = 0;
    ebuf[en] = 0;
}

int main(int argc, char **argv) {
    if (argc > 2 && strcmp(argv[1], "child") == 0)
        return child(argc, argv);

    const char *self = "/bin/spawnprobe";
    int out[2], err[2];
    check(pipe2(out, O_CLOEXEC) == 0 && pipe2(err, O_CLOEXEC) == 0, "pipes");
    int null = open("/dev/null", O_RDWR | O_CLOEXEC);
    check(null >= 0, "open /dev/null");
    /* An inherited descriptor (no O_CLOEXEC) at offset 1: the child reads "ELF". */
    int file = open(self, O_RDONLY);
    check(file >= 0 && lseek(file, 1, SEEK_SET) == 1, "open self at offset 1");
    check(chdir("/bin") == 0, "chdir /bin");

    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    posix_spawn_file_actions_adddup2(&fa, null, 0);
    posix_spawn_file_actions_adddup2(&fa, out[1], 1);
    posix_spawn_file_actions_adddup2(&fa, err[1], 2);
    char fdarg[16];
    snprintf(fdarg, sizeof fdarg, "%d", file);
    char *cargv[] = {"spawnprobe", "child", "7", fdarg, NULL};
    char *cenv[] = {"SPAWNPROBE=through-envp", NULL};
    pid_t pid = -1;
    int rc = posix_spawn(&pid, self, &fa, NULL, cargv, cenv);
    check(rc == 0 && pid > 0, "posix_spawn self");
    close(out[1]);
    close(err[1]);
    char obuf[512], ebuf[256];
    drain(out[0], err[0], obuf, sizeof obuf, ebuf, sizeof ebuf);
    int status = -1;
    check(waitpid(pid, &status, 0) == pid, "waitpid");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 7, "exit status 7");
    check(strcmp(obuf, "child argc=4 arg=7 env=through-envp stdin=0 cwd=/bin fd=ELF\n") == 0,
          "child stdout");
    check(strcmp(ebuf, "child stderr line\n") == 0, "child stderr");
    if (failures)
        printf("spawnprobe: got stdout [%s] stderr [%s] status %#x\n", obuf, ebuf, status);

    /* The parent's own descriptors are untouched by the child's dup2s. */
    check(fcntl(0, F_GETFD) >= 0 && fcntl(1, F_GETFD) >= 0, "parent stdio intact");
    check(lseek(file, 0, SEEK_CUR) >= 1, "parent file still open");

    /* A program that is not there: posix_spawn itself reports ENOENT. */
    pid_t none = -1;
    rc = posix_spawn(&none, "/bin/no-such-program", NULL, NULL, cargv, environ);
    check(rc == ENOENT, "missing program is ENOENT");

    /* A second child, inheriting stdout, exiting 0; waitpid(-1) reaps it. */
    char *zargv[] = {"spawnprobe", "child", "0", NULL};
    rc = posix_spawn(&pid, self, NULL, NULL, zargv, environ);
    check(rc == 0, "posix_spawn inheriting stdout");
    status = -1;
    check(waitpid(-1, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "waitpid(-1) exit 0");
    check(waitpid(-1, &status, WNOHANG) < 0 && errno == ECHILD, "no children left");

    printf("spawnprobe: %s (%d failures)\n", failures ? "FAIL" : "OK", failures);
    return failures ? 1 : 0;
}
