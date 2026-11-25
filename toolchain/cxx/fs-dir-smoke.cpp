//J941_r07_001
// SH9-c3: libc++ directory iteration over the volume this process was granted. The launch
// gets a fatfs session on the RAM FAT image, which it sees as "/"; the image builder always
// lays down /EFI/BOOT/BOOTAA64.EFI ("esp-ok") and /HELLO.TXT ("hello!"), so those are the
// fixed points the proof names. It only reads: no entry is created, renamed or removed.
// — TEAL
#include <dirent.h>
#include <filesystem>
#include <string.h>
#include <system_error>
#include <unistd.h>

namespace fs = std::filesystem;

static bool record(const char* text) {
    unsigned long length = 0;
    while (text[length] != '\0') ++length;
    return write(1, text, length) == static_cast<long>(length);
}

static int fail(const char* why) {
    record("SH9-c3: ");
    record(why);
    record("\nSH9-c3 libc++ directory iteration FAILED\n");
    return 1;
}

static bool dot(const char* name) {
    return strcmp(name, ".") == 0 || strcmp(name, "..") == 0;
}

static bool named(const fs::directory_entry& entry, const char* name) {
    return entry.path().filename().native() == name;
}

// Root names as relibc's own readdir reports them, "." and ".." left out as libc++ does.
enum { MAX_NAMES = 64, NAME_BYTES = 256 };
static char raw_names[MAX_NAMES][NAME_BYTES];
static bool raw_seen[MAX_NAMES];

static int raw_root_listing() {
    DIR* dir = opendir("/");
    if (dir == nullptr) return -1;
    int count = 0;
    while (struct dirent* entry = readdir(dir)) {
        if (dot(entry->d_name)) continue;
        if (count == MAX_NAMES || strlen(entry->d_name) >= NAME_BYTES) {
            closedir(dir);
            return -1;
        }
        strcpy(raw_names[count++], entry->d_name);
    }
    closedir(dir);
    return count;
}

extern "C" int main() {
    if (!record("SH9-c3 libc++ directory iteration begin\n")) return 10;
    std::error_code error;

    // 1. One level, both kinds of entry, and the stat-backed refresh behind file_size().
    int seen = 0;
    for (fs::directory_iterator it("/EFI", error), end; !error && it != end; it.increment(error)) {
        const fs::directory_entry& entry = *it;
        if (!named(entry, "BOOT") || entry.path() != fs::path("/EFI/BOOT")) return fail("/EFI entry name");
        if (!entry.is_directory(error) || error) return fail("/EFI/BOOT kind");
        ++seen;
    }
    if (error || seen != 1) return fail("/EFI entry count");
    seen = 0;
    for (const fs::directory_entry& entry : fs::directory_iterator("/EFI/BOOT")) {
        if (!named(entry, "BOOTAA64.EFI") || !entry.is_regular_file()) return fail("/EFI/BOOT entry");
        if (entry.file_size() != 6) return fail("/EFI/BOOT/BOOTAA64.EFI size");
        ++seen;
    }
    if (seen != 1) return fail("/EFI/BOOT entry count");
    if (!record("SH9-c3: directory_iterator over a subdirectory -> OK\n")) return 11;

    // 2. The volume root: libc++ yields exactly relibc's entries, each once.
    const int raw = raw_root_listing();
    if (raw <= 0) return fail("relibc root listing");
    int listed = 0;
    bool efi = false, hello = false;
    for (fs::directory_iterator it("/", error), end; !error && it != end; it.increment(error)) {
        const fs::directory_entry& entry = *it;
        const fs::path filename = entry.path().filename();
        const char* name = filename.c_str();
        if (dot(name) || entry.path().parent_path() != fs::path("/")) return fail("root entry path");
        int match = -1;
        for (int index = 0; index < raw; ++index) {
            if (!raw_seen[index] && strcmp(raw_names[index], name) == 0) {
                match = index;
                break;
            }
        }
        if (match < 0) {
            record("SH9-c3: unmatched root entry: ");
            record(name);
            record("\n");
            return fail("root entry not in relibc listing");
        }
        raw_seen[match] = true;
        ++listed;
        if (strcmp(name, "EFI") == 0) efi = entry.is_directory();
        if (strcmp(name, "HELLO.TXT") == 0) hello = entry.is_regular_file() && entry.file_size() == 6;
    }
    if (error || listed != raw || !efi || !hello) return fail("root listing");
    if (!record("SH9-c3: volume root matches relibc readdir -> OK\n")) return 12;

    // 3. Recursion: depth, order within a single-child chain, and disable_recursion_pending.
    fs::recursive_directory_iterator walk("/EFI", error), done;
    if (error || walk == done || walk.depth() != 0 || walk->path() != fs::path("/EFI/BOOT"))
        return fail("recursive first entry");
    walk.increment(error);
    if (error || walk == done || walk.depth() != 1 || walk->path() != fs::path("/EFI/BOOT/BOOTAA64.EFI"))
        return fail("recursive second entry");
    walk.increment(error);
    if (error || walk != done) return fail("recursive end");
    fs::recursive_directory_iterator shallow("/EFI");
    if (shallow == done || shallow->path() != fs::path("/EFI/BOOT")) return fail("shallow first entry");
    shallow.disable_recursion_pending();
    ++shallow;
    if (shallow != done) return fail("disable_recursion_pending");
    if (!record("SH9-c3: recursive_directory_iterator depth and pruning -> OK\n")) return 13;

    // 4. Errors: ENOENT and ENOTDIR through error_code, and ENOENT as a thrown filesystem_error.
    const fs::path missing("/SH9C3-MISSING");
    fs::directory_iterator absent(missing, error);
    if (error != std::errc::no_such_file_or_directory || absent != fs::directory_iterator())
        return fail("missing directory error_code");
    error.clear();
    fs::directory_iterator file("/HELLO.TXT", error);
    if (error != std::errc::not_a_directory || file != fs::directory_iterator())
        return fail("file as directory error_code");
    bool thrown = false;
    try {
        fs::directory_iterator boom(missing);
    } catch (const fs::filesystem_error& caught) {
        thrown = caught.code() == std::errc::no_such_file_or_directory && caught.path1() == missing;
    }
    if (!thrown) return fail("thrown filesystem_error");
    if (!record("SH9-c3: missing and non-directory iteration errors -> OK\n")) return 14;

    if (!record("SH9-c3 libc++ directory iteration green\n")) return 15;
    return 0;
}
