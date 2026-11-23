//J939_r05_001
#include <filesystem>
#include <unistd.h>

namespace fs = std::filesystem;

static bool record(const char* text) {
    unsigned long length = 0;
    while (text[length] != '\0') ++length;
    return write(1, text, length) == static_cast<long>(length);
}

static bool same(const fs::path& value, const char* expected) {
    return value.native() == expected;
}

static int fail() {
    record("SH9-c1 libc++ filesystem path FAILED\n");
    return 1;
}

extern "C" int main() {
    if (!record("SH9-c1 libc++ filesystem path begin\n")) return 10;
    const fs::path input("/alpha//beta/../gamma.tar.gz");
    if (!same(input.root_path(), "/") || !same(input.root_directory(), "/") ||
        !same(input.relative_path(), "alpha//beta/../gamma.tar.gz") ||
        !same(input.parent_path(), "/alpha//beta/..") || !same(input.filename(), "gamma.tar.gz") ||
        !same(input.stem(), "gamma.tar") || !same(input.extension(), ".gz")) return fail();
    if (!record("SH9-c1: decomposition, extension, and root -> OK\n")) return 11;

    if (!same(input.lexically_normal(), "/alpha/gamma.tar.gz") ||
        !same(fs::path("a/./b/../c").lexically_normal(), "a/c")) return fail();
    if (!record("SH9-c1: lexical normalization -> OK\n")) return 12;

    fs::path replaced = input;
    replaced.replace_extension(".o");
    fs::path joined("/root");
    joined /= "leaf";
    if (!same(replaced, "/alpha//beta/../gamma.tar.o") || !same(joined, "/root/leaf")) return fail();
    if (!record("SH9-c1: append and replace_extension -> OK\n")) return 13;

    unsigned long components = 0;
    for (const auto& component : input) {
        if (component.empty()) return fail();
        ++components;
    }
    if (components != 5) return fail();
    if (!record("SH9-c1: path iteration -> OK\n")) return 14;
    if (!record("SH9-c1 libc++ filesystem path green\n")) return 15;
    return 0;
}
