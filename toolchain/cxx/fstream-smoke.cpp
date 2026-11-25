//J942_r08_001
// SH9-c4: libc++ file streams over the volume this process was granted, with libc++'s
// locale layer switched on. The fatfs session on the RAM FAT image is "/"; the image builder
// always lays down /HELLO.TXT ("hello!") and /EFI/BOOT/BOOTAA64.EFI ("esp-ok"). The streams
// only read. The classic-locale number and time facets are exercised too, because they are
// what reach relibc's strtod_l, strtof_l, strtold_l and strftime_l. — TEAL
#include <ctime>
#include <fstream>
#include <iomanip>
#include <ios>
#include <iterator>
#include <sstream>
#include <string>
#include <unistd.h>

static bool record(const char* text) {
    unsigned long length = 0;
    while (text[length] != '\0') ++length;
    return write(1, text, length) == static_cast<long>(length);
}

static int fail(const char* why) {
    record("SH9-c4: ");
    record(why);
    record("\nSH9-c4 libc++ file streams FAILED\n");
    return 1;
}

extern "C" int main() {
    if (!record("SH9-c4 libc++ file streams begin\n")) return 10;

    // 1. A text fixture, line-wise and through the stream buffer.
    {
        std::ifstream text("/HELLO.TXT");
        if (!text.is_open()) return fail("open /HELLO.TXT");
        std::string line;
        if (!std::getline(text, line) || line != "hello!") return fail("getline /HELLO.TXT");
        if (std::getline(text, line) || !text.eof()) return fail("end of /HELLO.TXT");
        std::ifstream again("/HELLO.TXT");
        const std::string whole{std::istreambuf_iterator<char>(again), std::istreambuf_iterator<char>()};
        if (whole != "hello!") return fail("istreambuf_iterator /HELLO.TXT");
    }
    if (!record("SH9-c4: ifstream reads a text fixture -> OK\n")) return 11;

    // 2. Binary mode: size by seeking to the end, a read from the middle, a full read, and a
    //    read past the end.
    {
        std::ifstream binary("/EFI/BOOT/BOOTAA64.EFI", std::ios::binary);
        if (!binary.is_open()) return fail("open BOOTAA64.EFI");
        binary.seekg(0, std::ios::end);
        if (binary.tellg() != std::streampos(6)) return fail("tellg at end");
        char tail[2] = {};
        binary.seekg(4);
        if (!binary.read(tail, 2) || tail[0] != 'o' || tail[1] != 'k') return fail("read at offset 4");
        char all[8] = {};
        binary.seekg(0);
        if (!binary.read(all, 6) || binary.gcount() != 6 || std::string(all, 6) != "esp-ok")
            return fail("full read");
        if (binary.read(all, 1) || binary.gcount() != 0 || !binary.eof()) return fail("read past end");
    }
    if (!record("SH9-c4: binary read, seek and tell -> OK\n")) return 12;

    // 3. A missing file: a failed open, then the same open with failbit as an exception.
    {
        std::ifstream missing("/SH9C4-MISSING.TXT");
        if (missing.is_open() || !missing.fail()) return fail("missing open state");
        bool thrown = false;
        std::ifstream strict;
        strict.exceptions(std::ios::failbit);
        try {
            strict.open("/SH9C4-MISSING.TXT");
        } catch (const std::ios_base::failure&) {
            thrown = true;
        }
        if (!thrown) return fail("failbit exception");
    }
    if (!record("SH9-c4: missing-file failure and ios_base::failure -> OK\n")) return 13;

    // 4. The classic locale's facets: number extraction (strtod_l, strtof_l, strtold_l),
    //    number insertion, and put_time (strftime_l).
    {
        std::istringstream numbers("3.25 -7 2.5e1 0.5");
        double d = 0;
        int i = 0;
        float f = 0;
        long double ld = 0;
        if (!(numbers >> d >> i >> f >> ld) || d != 3.25 || i != -7 || f != 25.0f || ld != 0.5L)
            return fail("number extraction");
        std::ostringstream out;
        out << 42 << ' ' << 2.5;
        if (out.str() != "42 2.5") return fail("number insertion");
        std::tm when{};
        when.tm_year = 2026 - 1900;
        when.tm_mon = 10;
        when.tm_mday = 25;
        when.tm_hour = 12;
        when.tm_min = 34;
        when.tm_sec = 56;
        std::ostringstream stamp;
        stamp << std::put_time(&when, "%Y-%m-%d %H:%M:%S");
        if (stamp.str() != "2026-11-25 12:34:56") return fail("put_time");
    }
    if (!record("SH9-c4: classic-locale number and time facets -> OK\n")) return 14;

    if (!record("SH9-c4 libc++ file streams green\n")) return 15;
    return 0;
}
