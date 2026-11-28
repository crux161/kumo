//J953_r19_001
// SH9-p1: LLVM's process layer (llvm::sys::ExecuteAndWait, ExecuteNoWait, Wait) launching a
// native KUMO program the way clang's driver launches its tools: posix_spawn, then waitpid.
// The child is the Rust std smoke on the RAM FAT image (/SMOKE.ELF spawnchild): it reads
// stdin to the end, prints "child says hi: " plus what it read, and exits 7. Its stdin is
// redirected to /dev/null; stdout/stderr are this process's own. Children get no filesystem
// grant, so a redirect to a file is refused cleanly at spawn time. — TEAL
#include "llvm/ADT/StringRef.h"
#include "llvm/Support/ErrorOr.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/Program.h"
#include "llvm/Support/raw_ostream.h"

#include <optional>
#include <string>

using namespace llvm;

static int fail(const std::string& why) {
    outs() << "SH9-p1: " << why << "\nSH9-p1 LLVM process launch FAILED\n";
    outs().flush();
    return 1;
}

extern "C" int main(int argc, char** argv) {
    outs() << "SH9-p1 LLVM process launch begin\n";
    outs().flush();
    const StringRef program = "/SMOKE.ELF";
    const StringRef args[] = {"SMOKE.ELF", "spawnchild"};
    const std::optional<StringRef> quiet[] = {StringRef(""), std::nullopt, std::nullopt};

    // 1. The program is on the granted volume and LLVM can find it.
    {
        if (!sys::fs::exists(program)) return fail("/SMOKE.ELF is not on the volume");
        ErrorOr<std::string> found = sys::findProgramByName("SMOKE.ELF", {"/BIN", "/"});
        if (!found || *found != program) return fail("findProgramByName");
    }
    outs() << "SH9-p1: findProgramByName(SMOKE.ELF) = /SMOKE.ELF -> OK\n";

    // 2. ExecuteAndWait: spawn, wait, exit status.
    {
        std::string error;
        bool failed = true;
        outs().flush();
        const int code = sys::ExecuteAndWait(program, args, std::nullopt, quiet, 0, 0, &error, &failed);
        if (failed || code != 7) return fail("ExecuteAndWait = " + std::to_string(code) + " (" + error + ")");
    }
    outs() << "\nSH9-p1: ExecuteAndWait -> exit 7 -> OK\n";

    // 3. ExecuteNoWait, then a polling Wait (WNOHANG) until the child is reaped.
    {
        std::string error;
        bool failed = true;
        outs().flush();
        sys::ProcessInfo child = sys::ExecuteNoWait(program, args, std::nullopt, quiet, 0, &error, &failed);
        if (failed || child.Pid <= 0) return fail("ExecuteNoWait (" + error + ")");
        sys::ProcessInfo done;
        unsigned polls = 0;
        do {
            ++polls;
            done = sys::Wait(child, 0, &error, nullptr, /*Polling=*/true);
        } while (done.Pid == 0 && polls < 10000000);
        if (done.Pid != child.Pid || done.ReturnCode != 7)
            return fail("polling Wait = pid " + std::to_string(done.Pid) + " code " + std::to_string(done.ReturnCode) +
                        " (" + error + ")");
        std::string again;
        if (sys::Wait(child, std::nullopt, &again).ReturnCode != -1) return fail("a reaped child was reaped twice");
    }
    outs() << "\nSH9-p1: ExecuteNoWait + polling Wait -> exit 7 -> OK\n";

    // 4. A blocking Wait on an unreaped child.
    {
        std::string error;
        bool failed = true;
        outs().flush();
        sys::ProcessInfo child = sys::ExecuteNoWait(program, args, std::nullopt, quiet, 0, &error, &failed);
        if (failed) return fail("ExecuteNoWait (" + error + ")");
        sys::ProcessInfo done = sys::Wait(child, std::nullopt, &error);
        if (done.Pid != child.Pid || done.ReturnCode != 7) return fail("blocking Wait (" + error + ")");
    }
    outs() << "\nSH9-p1: ExecuteNoWait + blocking Wait -> exit 7 -> OK\n";

    // 5. Clean refusals: a missing program, and a redirect to a file the child could not reach.
    {
        std::string error;
        bool failed = false;
        int code = sys::ExecuteAndWait("/NOPE.ELF", {"NOPE.ELF"}, std::nullopt, {}, 0, 0, &error, &failed);
        if (!failed || code != -1 || StringRef(error).find("No such file") == StringRef::npos)
            return fail("missing program: " + error);
        outs() << "SH9-p1: refused: " << error << "\n";
        error.clear();
        failed = false;
        const std::optional<StringRef> to_file[] = {StringRef(""), StringRef("/SH9P1.OUT"), std::nullopt};
        code = sys::ExecuteAndWait(program, args, std::nullopt, to_file, 0, 0, &error, &failed);
        if (!failed || code != -1 || StringRef(error).find("posix_spawn failed") == StringRef::npos)
            return fail("file redirect: " + error);
        outs() << "SH9-p1: refused: " << error << "\n";
    }
    outs() << "SH9-p1: missing program and file redirect refused cleanly -> OK\n";

    outs() << "SH9-p1 LLVM process launch green\n";
    outs().flush();
    (void)argc;
    (void)argv;
    return 0;
}
