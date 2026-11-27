//J948_r14_001
// SH9-d1: LLVM's Support and Demangle libraries (llvmorg-22.1.8) running natively on KUMO, used
// the way LLVM's own tools use them. Built against the installed libc++ layer and relibc, run
// with a fatfs session on the RAM FAT image as "/" (nothing it writes outlives the boot). — TEAL
#include "llvm/ADT/APFloat.h"
#include "llvm/ADT/APInt.h"
#include "llvm/ADT/DenseMap.h"
#include "llvm/ADT/SmallString.h"
#include "llvm/ADT/SmallVector.h"
#include "llvm/ADT/StringMap.h"
#include "llvm/ADT/StringExtras.h"
#include "llvm/ADT/StringRef.h"
#include "llvm/ADT/Twine.h"
#include "llvm/Demangle/Demangle.h"
#include "llvm/Support/BLAKE3.h"
#include "llvm/Support/CommandLine.h"
#include "llvm/Support/ConvertUTF.h"
#include "llvm/Support/Error.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/FormatVariadic.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Path.h"
#include "llvm/Support/Regex.h"
#include "llvm/Support/SHA256.h"
#include "llvm/Support/ThreadPool.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Support/xxhash.h"

#include <atomic>
#include <string>

using namespace llvm;

static int fail(const char* why) {
    outs() << "SH9-d1: " << why << "\nSH9-d1 LLVM Support FAILED\n";
    outs().flush();
    return 1;
}

static std::string hex(ArrayRef<uint8_t> bytes) {
    std::string out;
    raw_string_ostream stream(out);
    for (uint8_t byte : bytes) stream << format_hex_no_prefix(byte, 2);
    return out;
}

static cl::opt<int> Jobs("jobs", cl::desc("worker threads"), cl::init(1));
static cl::opt<std::string> Name("name", cl::desc("a name"), cl::init("none"));

extern "C" int main(int argc, char** argv) {
    outs() << "SH9-d1 LLVM Support begin\n";

    // 1. ADT, APInt/APFloat and formatv.
    {
        SmallVector<int, 4> values;
        for (int index = 0; index < 100; ++index) values.push_back(index * index);
        DenseMap<int, StringRef> names;
        names[1] = "one";
        names[42] = "answer";
        StringMap<unsigned> counts;
        for (StringRef word : {"llvm", "kumo", "llvm"}) ++counts[word];
        SmallVector<StringRef, 4> parts;
        StringRef("a, b ,c").split(parts, ',');
        APInt big = APInt(128, 1).shl(100);
        APFloat third(1.0);
        third.divide(APFloat(3.0), APFloat::rmNearestTiesToEven);
        if (values.size() != 100 || values[99] != 9801) return fail("SmallVector");
        if (names.lookup(42) != "answer" || counts["llvm"] != 2) return fail("DenseMap/StringMap");
        if (parts.size() != 3 || parts[1].trim() != "b") return fail("StringRef::split");
        const std::string power = toString(big, 10, false);
        if (power != "1267650600228229401496703205376") return fail(("APInt 2^100 = " + power).c_str());
        const std::string formatted = formatv("{0:x}|{1,5}|{2:F2}", 255, "kumo", third.convertToDouble()).str();
        if (formatted != "0xff| kumo|0.33") return fail(("formatv = " + formatted).c_str());
        if ((Twine("SH9-") + "d" + Twine(1)).str() != "SH9-d1") return fail("Twine");
    }
    outs() << "SH9-d1: ADT, APInt, APFloat and formatv -> OK\n";

    // 2. CommandLine and Error/Expected.
    {
        const char* fake[] = {"support-smoke", "-jobs=4", "-name=kumo"};
        if (!cl::ParseCommandLineOptions(3, fake, "SH9-d1", &errs()) || Jobs != 4 || Name != "kumo")
            return fail("cl::ParseCommandLineOptions");
        auto parse = [](StringRef text) -> Expected<int> {
            int value = 0;
            if (text.getAsInteger(10, value))
                return createStringError(std::errc::invalid_argument, "not a number: %s", text.str().c_str());
            return value;
        };
        Expected<int> good = parse("42");
        Expected<int> bad = parse("x");
        if (!good || *good != 42 || bad) return fail("Expected value");
        if (toString(bad.takeError()) != "not a number: x") return fail("Expected error text");
    }
    outs() << "SH9-d1: CommandLine and Error/Expected -> OK\n";

    // 3. sys::path and sys::fs on the granted volume, MemoryBuffer reading it back.
    {
        SmallString<64> dir("/SH9D1");
        sys::path::append(dir, "A", "B");
        if (sys::path::filename("/x/y/F.TXT") != "F.TXT" || sys::path::parent_path(dir) != "/SH9D1/A" ||
            sys::path::extension("F.TXT") != ".TXT")
            return fail("sys::path");
        if (sys::fs::exists("/SH9D1")) return fail("/SH9D1 left over");
        if (std::error_code error = sys::fs::create_directories(dir)) return fail("create_directories");
        SmallString<64> file(dir);
        sys::path::append(file, "F.TXT");
        {
            std::error_code error;
            raw_fd_ostream out(file, error, sys::fs::OF_None);
            if (error) return fail("raw_fd_ostream open");
            out << "llvm on kumo\n";
        }
        uint64_t size = 0;
        if (sys::fs::file_size(file, size) || size != 13 || !sys::fs::is_regular_file(file)) return fail("file_size");
        ErrorOr<std::unique_ptr<MemoryBuffer>> buffer = MemoryBuffer::getFile(file);
        if (!buffer || (*buffer)->getBuffer() != "llvm on kumo\n") return fail("MemoryBuffer::getFile");
        int entries = 0;
        std::error_code walk;
        for (sys::fs::directory_iterator it(dir, walk), end; !walk && it != end; it.increment(walk)) ++entries;
        if (walk || entries != 1) return fail("sys::fs::directory_iterator");
        if (sys::fs::remove_directories("/SH9D1") || sys::fs::exists("/SH9D1")) return fail("remove_directories");
    }
    outs() << "SH9-d1: sys::path, sys::fs and MemoryBuffer on the volume -> OK\n";

    // 4. Hashes against published vectors, and Regex.
    {
        if (hex(SHA256::hash(arrayRefFromStringRef("abc"))) !=
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
            return fail("SHA256");
        std::array<uint8_t, 32> digest = BLAKE3::hash<32>(ArrayRef<uint8_t>());
        if (hex(digest) != "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262") return fail("BLAKE3");
        if (xxHash64(StringRef()) != 0xEF46DB3751D8E999ULL) return fail("xxHash64");
        Regex pattern("^SH9-d([0-9])-([a-z]+)$");
        SmallVector<StringRef, 3> matches;
        if (!pattern.isValid() || !pattern.match("SH9-d1-llvm", &matches) || matches[1] != "1" || matches[2] != "llvm")
            return fail("Regex");
    }
    outs() << "SH9-d1: SHA256, BLAKE3, xxHash64 and Regex -> OK\n";

    // 5. A thread pool, the Itanium demangler, and UTF-8/wide conversion.
    {
        DefaultThreadPool pool(hardware_concurrency(4));
        std::atomic<int> sum{0};
        for (int task = 1; task <= 100; ++task) pool.async([&sum, task] { sum += task; });
        pool.wait();
        if (sum != 5050) return fail("ThreadPool");
        if (demangle("_ZN4kumo4bootEv") != "kumo::boot()" ||
            demangle("_ZNSt3__16vectorIiNS_9allocatorIiEEE9push_backEOi") !=
                "std::__1::vector<int, std::__1::allocator<int>>::push_back(int&&)")
            return fail("demangle");
        std::wstring wide;
        std::string back;
        if (!ConvertUTF8toWide("h\xc3\xa9llo", wide) || wide.size() != 5 || wide[1] != L'é' ||
            !convertWideToUTF8(wide, back) || back != "h\xc3\xa9llo")
            return fail("ConvertUTF wide");
    }
    outs() << "SH9-d1: ThreadPool, demangle and ConvertUTF -> OK\n";

    outs() << "SH9-d1 LLVM Support green\n";
    outs().flush();
    (void)argc;
    (void)argv;
    return 0;
}
