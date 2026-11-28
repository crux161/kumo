//J956_r22_001
// PROPOSAL/027 P2c: a granted child's stdio redirected to files, the way LLVM's ExecuteAndWait and
// a shell's `<in >out 2>&1` ask for it. The child is the Rust std smoke on the RAM FAT image
// (/SMOKE.ELF spawnchild): it reads stdin to the end, prints "child says hi: " plus what it read
// in upper case, writes "child stderr\n" to stderr, and exits 7. Nothing outlives the boot. — TEAL
#include "llvm/ADT/StringRef.h"
#include "llvm/Support/Program.h"
#include "llvm/Support/raw_ostream.h"

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>

#include <fcntl.h>
#include <spawn.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

extern "C" char** environ;

using namespace llvm;

namespace {

const char* const IN = "/P2C/IN.TXT";
const char* const CHILD_SAYS = "child says hi: PING FROM A FILE\n";

int fail(const std::string& why) {
    outs() << "P2c: " << why << " (errno " << errno << ")\nP2c spawn redirects FAILED\n";
    outs().flush();
    return 1;
}

std::string slurp(const char* path) {
    std::string out;
    const int fd = open(path, O_RDONLY);
    if (fd < 0) return "<missing>";
    char buffer[256];
    for (ssize_t n; (n = read(fd, buffer, sizeof buffer)) > 0;) out.append(buffer, static_cast<size_t>(n));
    close(fd);
    return out;
}

bool put(const char* path, const std::string& bytes) {
    const int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return false;
    const bool ok = write(fd, bytes.data(), bytes.size()) == static_cast<ssize_t>(bytes.size());
    return close(fd) == 0 && ok;
}

// posix_spawn /SMOKE.ELF spawnchild with `actions`; returns posix_spawn's error, and the exit
// status in `status` when it started.
int spawn_child(posix_spawn_file_actions_t* actions, int* status) {
    char arg0[] = "SMOKE.ELF";
    char arg1[] = "spawnchild";
    char* argv[] = {arg0, arg1, nullptr};
    pid_t pid = 0;
    const int error = posix_spawn(&pid, "/SMOKE.ELF", actions, nullptr, argv, environ);
    if (error == 0 && waitpid(pid, status, 0) != pid) return -1;
    return error;
}

}  // namespace

extern "C" int main(int argc, char** argv) {
    outs() << "P2c spawn redirects begin\n";
    outs().flush();
    if (mkdir("/P2C", 0755) != 0 || !put(IN, "ping from a file\n")) return fail("setup /P2C");

    // 1. One open file description behind dup'd descriptors (the 2>&1 prerequisite).
    {
        const int fd = open("/P2C/DUP.TXT", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        const int alias = fd < 0 ? -1 : dup(fd);
        if (alias < 0) return fail("open/dup");
        if (write(fd, "ab", 2) != 2 || write(alias, "cd", 2) != 2) return fail("write through both");
        if (lseek(alias, 0, SEEK_CUR) != 4) return fail("the alias did not see the shared offset");
        if (close(fd) != 0 || write(alias, "ef", 2) != 2 || close(alias) != 0)
            return fail("the alias stopped working when the other descriptor closed");
        if (slurp("/P2C/DUP.TXT") != "abcdef") return fail("DUP.TXT = " + slurp("/P2C/DUP.TXT"));
    }
    outs() << "P2c: dup'd descriptors share one offset and outlive each other's close -> OK\n";

    // 2. LLVM: stdin, stdout and stderr each from/to their own file.
    if (setenv("KUMO_GRANT", "fs", 1) != 0) return fail("setenv");
    const StringRef program = "/SMOKE.ELF";
    const StringRef args[] = {"SMOKE.ELF", "spawnchild"};
    {
        const std::optional<StringRef> files[] = {StringRef(IN), StringRef("/P2C/OUT.TXT"), StringRef("/P2C/ERR.TXT")};
        std::string error;
        bool failed = true;
        const int code = sys::ExecuteAndWait(program, args, std::nullopt, files, 0, 0, &error, &failed);
        if (failed || code != 7) return fail("ExecuteAndWait = " + std::to_string(code) + " (" + error + ")");
        if (slurp("/P2C/OUT.TXT") != CHILD_SAYS) return fail("OUT.TXT = " + slurp("/P2C/OUT.TXT"));
        if (slurp("/P2C/ERR.TXT") != "child stderr\n") return fail("ERR.TXT = " + slurp("/P2C/ERR.TXT"));
    }
    outs() << "P2c: ExecuteAndWait <IN >OUT 2>ERR -> exit 7, both files exact -> OK\n";

    // 3. LLVM: stdout and stderr to the same file is LLVM's dup2(1, 2), so they share an offset.
    {
        const std::optional<StringRef> files[] = {StringRef(IN), StringRef("/P2C/BOTH.TXT"), StringRef("/P2C/BOTH.TXT")};
        std::string error;
        bool failed = true;
        const int code = sys::ExecuteAndWait(program, args, std::nullopt, files, 0, 0, &error, &failed);
        if (failed || code != 7) return fail("ExecuteAndWait 2>&1 = " + std::to_string(code) + " (" + error + ")");
        const std::string both = slurp("/P2C/BOTH.TXT");
        if (both != std::string(CHILD_SAYS) + "child stderr\n") return fail("BOTH.TXT = " + both);
    }
    outs() << "P2c: ExecuteAndWait >BOTH 2>&1 -> stdout then stderr in one file -> OK\n";

    // 4. posix_spawn: O_TRUNC happens once, in the parent; O_APPEND reaches the child.
    {
        if (!put("/P2C/TRUNC.TXT", std::string(100, 'x'))) return fail("prefill TRUNC.TXT");
        posix_spawn_file_actions_t actions;
        posix_spawn_file_actions_init(&actions);
        posix_spawn_file_actions_addopen(&actions, 0, IN, O_RDONLY, 0);
        posix_spawn_file_actions_addopen(&actions, 1, "/P2C/TRUNC.TXT", O_WRONLY | O_CREAT | O_TRUNC, 0644);
        posix_spawn_file_actions_addopen(&actions, 2, "/dev/null", O_WRONLY, 0);
        int status = 0;
        int error = spawn_child(&actions, &status);
        posix_spawn_file_actions_destroy(&actions);
        if (error != 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 7) return fail("O_TRUNC spawn");
        if (slurp("/P2C/TRUNC.TXT") != CHILD_SAYS) return fail("TRUNC.TXT = " + slurp("/P2C/TRUNC.TXT"));
        for (int round = 0; round < 2; ++round) {
            posix_spawn_file_actions_init(&actions);
            posix_spawn_file_actions_addopen(&actions, 0, IN, O_RDONLY, 0);
            posix_spawn_file_actions_addopen(&actions, 1, "/P2C/APP.TXT", O_WRONLY | O_CREAT | O_APPEND, 0644);
            posix_spawn_file_actions_addopen(&actions, 2, "/dev/null", O_WRONLY, 0);
            error = spawn_child(&actions, &status);
            posix_spawn_file_actions_destroy(&actions);
            if (error != 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 7) return fail("O_APPEND spawn");
        }
        if (slurp("/P2C/APP.TXT") != std::string(CHILD_SAYS) + CHILD_SAYS) return fail("APP.TXT = " + slurp("/P2C/APP.TXT"));
    }
    outs() << "P2c: posix_spawn O_TRUNC truncates once, O_APPEND appends twice -> OK\n";

    // 5. Errors come back from posix_spawn itself, before any child exists.
    {
        posix_spawn_file_actions_t actions;
        int status = 0;
        posix_spawn_file_actions_init(&actions);
        posix_spawn_file_actions_addopen(&actions, 0, "/P2C/MISSING.TXT", O_RDONLY, 0);
        const int missing = spawn_child(&actions, &status);
        posix_spawn_file_actions_destroy(&actions);
        posix_spawn_file_actions_init(&actions);
        posix_spawn_file_actions_addopen(&actions, 1, "/NODIR/X.TXT", O_WRONLY | O_CREAT, 0644);
        const int no_dir = spawn_child(&actions, &status);
        posix_spawn_file_actions_destroy(&actions);
        if (missing != ENOENT || no_dir != ENOENT)
            return fail("errors " + std::to_string(missing) + "/" + std::to_string(no_dir));
    }
    outs() << "P2c: a missing input and a missing directory fail posix_spawn with ENOENT -> OK\n";

    // 6. A read-only grant: a writable redirect is refused (EACCES); an input redirect works.
    //    This child's stdout is ours, so its greeting is the only one on the console.
    {
        if (setenv("KUMO_GRANT", "fs-ro", 1) != 0) return fail("setenv fs-ro");
        posix_spawn_file_actions_t actions;
        int status = 0;
        posix_spawn_file_actions_init(&actions);
        posix_spawn_file_actions_addopen(&actions, 1, "/P2C/RO.TXT", O_WRONLY | O_CREAT, 0644);
        const int write_error = spawn_child(&actions, &status);
        posix_spawn_file_actions_destroy(&actions);
        struct stat st {};
        if (write_error != EACCES || stat("/P2C/RO.TXT", &st) == 0) return fail("fs-ro writable redirect");
        posix_spawn_file_actions_init(&actions);
        posix_spawn_file_actions_addopen(&actions, 0, IN, O_RDONLY, 0);
        outs().flush();
        const int read_error = spawn_child(&actions, &status);
        posix_spawn_file_actions_destroy(&actions);
        if (read_error != 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 7) return fail("fs-ro input redirect");
    }
    outs() << "P2c: a read-only grant refuses a writable redirect (EACCES) and reads its input -> OK\n";

    // 7. No marker, no filesystem for the child: a file redirect is refused (unchanged).
    {
        if (unsetenv("KUMO_GRANT") != 0) return fail("unsetenv");
        const std::optional<StringRef> files[] = {StringRef(""), StringRef("/P2C/NOGRANT.TXT"), std::nullopt};
        std::string error;
        bool failed = false;
        const int code = sys::ExecuteAndWait(program, args, std::nullopt, files, 0, 0, &error, &failed);
        if (!failed || code != -1 || StringRef(error).find("Operation not supported") == StringRef::npos)
            return fail("ungranted redirect: " + error);
    }
    outs() << "P2c: without a grant a file redirect is still refused -> OK\n";

    for (const char* name : {"IN", "DUP", "OUT", "ERR", "BOTH", "TRUNC", "APP"}) {
        const std::string path = std::string("/P2C/") + name + ".TXT";
        if (unlink(path.c_str()) != 0) return fail("unlink " + path);
    }
    if (rmdir("/P2C") != 0) return fail("rmdir /P2C");
    outs() << "P2c: files removed -> OK\nP2c spawn redirects green\n";
    outs().flush();
    (void)argc;
    (void)argv;
    return 0;
}
