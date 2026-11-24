//J940_r06_001
#include <filesystem>
#include <system_error>
#include <unistd.h>

namespace fs = std::filesystem;

static bool record(const char* text) {
    unsigned long length = 0;
    while (text[length] != '\0') ++length;
    return write(1, text, length) == static_cast<long>(length);
}

static int fail() {
    record("SH9-c2 libc++ filesystem status FAILED\n");
    return 1;
}

extern "C" int main() {
    if (!record("SH9-c2 libc++ filesystem status begin\n")) return 10;

    std::error_code error;
    const fs::file_status root = fs::status("/", error);
    if (error || !fs::exists(root) || !fs::is_directory(root)) return fail();
    if (!record("SH9-c2: root directory status -> OK\n")) return 11;

    const fs::file_status device = fs::status("/dev/null", error);
    if (error || !fs::exists(device) || !fs::is_character_file(device)) return fail();
    if (!record("SH9-c2: visible device-file status -> OK\n")) return 12;

    const fs::path missing("/SH9C2-MISSING");
    const fs::file_status absent = fs::status(missing, error);
    if (absent.type() != fs::file_type::not_found ||
        error != std::errc::no_such_file_or_directory || fs::exists(absent)) return fail();
    error.clear();
    if (fs::exists(missing, error) || error) return fail();
    if (!record("SH9-c2: missing VFS path and exists semantics -> OK\n")) return 13;

    if (!record("SH9-c2 libc++ filesystem status green\n")) return 14;
    return 0;
}
