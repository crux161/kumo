//J944_r10_001
// SH9-c5: libc++ writes. ofstream and the mutating std::filesystem operations, on the volume
// this process was granted: a fatfs session on the RAM FAT image, seen as "/". The image lives
// in memory, so nothing here outlives the boot. Every name is 8.3 upper case so the proof does
// not depend on long-name writes. The tree is built under /SH9C5 and removed again; a leftover
// would fail the first step of the next run. — TEAL
#include <filesystem>
#include <fstream>
#include <iterator>
#include <string>
#include <system_error>
#include <unistd.h>

namespace fs = std::filesystem;

static bool record(const char* text) {
    unsigned long length = 0;
    while (text[length] != '\0') ++length;
    return write(1, text, length) == static_cast<long>(length);
}

static int fail(const char* why) {
    record("SH9-c5: ");
    record(why);
    record("\nSH9-c5 libc++ filesystem writes FAILED\n");
    return 1;
}

static std::string slurp(const fs::path& path) {
    std::ifstream in(path, std::ios::binary);
    return std::string{std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}

extern "C" int main() {
    if (!record("SH9-c5 libc++ filesystem writes begin\n")) return 10;
    const fs::path root("/SH9C5");
    const fs::path deep = root / "A" / "B";
    const fs::path file = deep / "F.TXT";
    std::error_code error;

    // 1. Directories: create, create again, create a chain, and refuse a missing parent.
    if (fs::exists(root, error)) return fail("/SH9C5 left over from an earlier run");
    if (!fs::create_directory(root, error) || error) return fail("create_directory");
    if (fs::create_directory(root, error) || error) return fail("create_directory on an existing directory");
    if (!fs::create_directories(deep, error) || error || !fs::is_directory(deep)) return fail("create_directories");
    if (fs::create_directory(root / "NO" / "SUCH", error) || error != std::errc::no_such_file_or_directory)
        return fail("create_directory under a missing parent");
    if (!record("SH9-c5: directories created, existing and missing parents -> OK\n")) return 11;

    // 2. ofstream: write, append, read back, then shrink with resize_file.
    {
        std::ofstream out(file, std::ios::binary);
        if (!out.is_open() || !(out << "kumo\n") || !out.flush()) return fail("ofstream write");
    }
    {
        std::ofstream more(file, std::ios::binary | std::ios::app);
        if (!more.is_open() || !(more << "c5\n")) return fail("ofstream append");
    }
    if (slurp(file) != "kumo\nc5\n" || fs::file_size(file) != 8) return fail("written contents");
    {
        std::ofstream again(file, std::ios::binary | std::ios::trunc);
        if (!again.is_open() || !(again << "truncated")) return fail("ofstream trunc");
    }
    if (slurp(file) != "truncated") return fail("trunc contents");
    fs::resize_file(file, 4, error);
    if (error || fs::file_size(file) != 4 || slurp(file) != "trun") return fail("resize_file");
    if (!record("SH9-c5: ofstream write, append, trunc and resize_file -> OK\n")) return 12;

    // 3. copy_file: a copy, a refused overwrite, an allowed overwrite.
    const fs::path copy = root / "COPY.TXT";
    if (!fs::copy_file(file, copy, error) || error || slurp(copy) != "trun") return fail("copy_file");
    {
        std::ofstream other(file, std::ios::binary | std::ios::trunc);
        other << "new";
    }
    if (fs::copy_file(file, copy, error) || error != std::errc::file_exists) return fail("copy_file refusal");
    error.clear();
    const bool overwrote = fs::copy_file(file, copy, fs::copy_options::overwrite_existing, error);
    if (!overwrote || error || slurp(copy) != "new") {
        record("SH9-c5: overwrite errno=");
        const std::string detail = std::to_string(error.value()) + " copy=\"" + slurp(copy) + "\" source=\"" +
                                   slurp(file) + "\"\n";
        record(detail.c_str());
        return fail("copy_file overwrite");
    }
    // st_dev/st_ino name one file (J944_r10_001): another spelling of the same FAT name is the
    // same file, and the copy is not.
    if (!fs::equivalent(file, "/sh9c5/a/b/f.txt") || fs::equivalent(file, copy) ||
        fs::status(file).type() != fs::file_type::regular)
        return fail("equivalent");
    if (!record("SH9-c5: copy_file, refusal, overwrite_existing and equivalent -> OK\n")) return 13;

    // 4. rename, then the directory shows exactly the new name.
    const fs::path renamed = deep / "G.TXT";
    fs::rename(file, renamed, error);
    if (error || fs::exists(file) || slurp(renamed) != "new") return fail("rename");
    int entries = 0;
    bool only_g = true;
    for (const fs::directory_entry& entry : fs::directory_iterator(deep)) {
        ++entries;
        only_g = only_g && entry.path().filename() == "G.TXT";
    }
    if (entries != 1 || !only_g) return fail("listing after rename");
    if (!record("SH9-c5: rename and the listing after it -> OK\n")) return 14;

    // 5. remove: a file, a missing name, a non-empty directory (refused), then remove_all.
    if (!fs::remove(copy, error) || error || fs::exists(copy)) return fail("remove file");
    if (fs::remove(copy, error) || error) return fail("remove of a missing name");
    if (fs::remove(deep, error) || !error) return fail("remove of a non-empty directory");
    error.clear();
    // Left: /SH9C5, A, B and G.TXT.
    const std::uintmax_t removed = fs::remove_all(root, error);
    if (error || removed != 4 || fs::exists(root)) return fail("remove_all");
    if (!record("SH9-c5: remove, non-empty refusal and remove_all -> OK\n")) return 15;

    if (!record("SH9-c5 libc++ filesystem writes green\n")) return 16;
    return 0;
}
