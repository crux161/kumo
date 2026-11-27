//J951_r17_001
// SH9-d4: native ELF and regular GNU archive readers. Inputs are host-assembled fixtures;
// generic binary/IR factories, writers and native assembly remain later slices. — KESTREL
#include "llvm/ADT/SmallString.h"
#include "llvm/Object/Archive.h"
#include "llvm/Object/ELFObjectFile.h"
#include "llvm/Support/Endian.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/raw_ostream.h"
#include "object-fixtures.h"

using namespace llvm;
using namespace llvm::object;

template <size_t N> static StringRef bytes(const unsigned char (&data)[N]) {
    return StringRef(reinterpret_cast<const char*>(data), N);
}

template <typename T> static bool ok(Expected<T>& result) {
    if (result) return true;
    errs() << "SH9-d4 detail: " << toString(result.takeError()) << '\n';
    return false;
}

static void record(StringRef text) {
    outs() << text << '\n';
    outs().flush();
}

static int fail(const char* why) {
    outs() << "SH9-d4: " << why << "\nSH9-d4 LLVM Object FAILED\n";
    outs().flush();
    return 1;
}

static bool rejectsELF(StringRef data) {
    auto result = ObjectFile::createELFObjectFile(MemoryBufferRef(data, "invalid.o"));
    if (result) return false;
    return !toString(result.takeError()).empty();
}

static bool archiveChecks(MemoryBufferRef buffer) {
    auto archive = Archive::create(buffer);
    if (!ok(archive)) return false;
    if ((*archive)->kind() != Archive::K_GNU || (*archive)->isThin() ||
        !(*archive)->hasSymbolTable() || (*archive)->getNumberOfSymbols() != 8) return false;
    Error error = Error::success();
    unsigned children = 0;
    bool good = true;
    for (const auto& child : (*archive)->children(error)) {
        auto name = child.getName();
        if (!ok(name)) { good = false; break; }
        auto memory = child.getMemoryBufferRef();
        if (!ok(memory)) { good = false; break; }
        auto elf = ObjectFile::createELFObjectFile(*memory);
        if (!ok(elf)) { good = false; break; }
        if (*name == "aarch64.o") {
            good &= (*elf)->getArch() == Triple::aarch64 && memory->getBuffer() == bytes(FixtureELF64LE);
        } else if (*name == "arm32-big-endian-long-member-name.o") {
            good &= (*elf)->getArch() == Triple::arm && isa<ELF32BEObjectFile>(**elf) &&
                    memory->getBuffer() == bytes(FixtureELF32BE);
        } else good = false;
        ++children;
    }
    if (error) { consumeError(std::move(error)); return false; }
    if (!good || children != 2) return false;
    unsigned answers = 0;
    for (const auto& symbol : (*archive)->symbols()) {
        auto member = symbol.getMember();
        if (!ok(member)) return false;
        if (symbol.getName() == "sh9_answer") ++answers;
    }
    auto found = (*archive)->findSym("sh9_answer");
    if (!ok(found) || !found->has_value() || answers != 2) return false;
    auto absent = (*archive)->findSym("absent_symbol");
    return ok(absent) && !absent->has_value();
}

static bool fileChecks(const char* path, StringRef contents, bool isArchive) {
    std::error_code error;
    raw_fd_ostream stream(path, error);
    if (error) return false;
    stream << contents;
    stream.close();
    if (stream.has_error()) return false;
    auto buffer = MemoryBuffer::getFile(path);
    if (!buffer || (*buffer)->getBuffer() != contents) return false;
    bool good;
    if (isArchive) good = archiveChecks((*buffer)->getMemBufferRef());
    else {
        auto object = ObjectFile::createELFObjectFile((*buffer)->getMemBufferRef());
        good = ok(object) && (*object)->getArch() == Triple::aarch64 && (*object)->isRelocatableObject();
    }
    return !sys::fs::remove(path) && good;
}

extern "C" int main() {
    record("SH9-d4 LLVM Object begin");
    auto result = ObjectFile::createELFObjectFile(MemoryBufferRef(bytes(FixtureELF64LE), "aarch64.o"));
    if (!ok(result)) return fail("ELF64LE creation");
    ObjectFile& object = **result;
    if (!isa<ELF64LEObjectFile>(object) || object.getArch() != Triple::aarch64 ||
        object.getBytesInAddress() != 8 || !object.isRelocatableObject() ||
        object.makeTriple().getArch() != Triple::aarch64) return fail("ELF64LE identity");
    bool text = false, data = false, rodata = false, bss = false;
    for (const auto& section : object.sections()) {
        auto name = section.getName();
        if (!ok(name)) return fail("section name");
        if (*name == ".bss") {
            bss = section.isBSS() && section.getSize() == 32 && section.getAlignment().value() == 16;
            continue;
        }
        if (*name == ".text" || *name == ".data" || *name == ".rodata") {
            auto contents = section.getContents();
            if (!ok(contents)) return fail("section contents");
            if (*name == ".text") text = section.isText() &&
                *contents == StringRef("\x40\x05\x80\xd2\xc0\x03\x5f\xd6", 8);
            if (*name == ".data") data = section.isData() && contents->size() == 8;
            if (*name == ".rodata") rodata = *contents == StringRef("kumo-object\0", 12);
        }
    }
    if (!text || !data || !rodata || !bss) return fail("ELF sections, bytes or BSS alignment");
    record("SH9-d4: ELF64LE identity, section bytes, kinds and BSS alignment -> OK");

    unsigned definitions = 0;
    bool function = false, missing = false;
    for (const auto& symbol : object.symbols()) {
        auto name = symbol.getName();
        if (!ok(name)) return fail("symbol name");
        auto flags = symbol.getFlags();
        if (!ok(flags)) return fail("symbol flags");
        if ((*flags & SymbolRef::SF_Global) && !(*flags & SymbolRef::SF_Undefined)) ++definitions;
        if (*name == "sh9_answer") {
            auto type = symbol.getType();
            if (!ok(type)) return fail("symbol type");
            auto address = symbol.getAddress();
            if (!ok(address)) return fail("symbol address");
            auto section = symbol.getSection();
            if (!ok(section) || *section == object.section_end()) return fail("symbol section");
            function = *type == SymbolRef::ST_Function && *address == 0 &&
                       ELFSymbolRef(symbol).getSize() == 8 && (*section)->isText();
        }
        if (*name == "sh9_missing") missing = *flags & SymbolRef::SF_Undefined;
    }
    if (!function || !missing || definitions != 4) return fail("defined/undefined symbol semantics");
    record("SH9-d4: symbols, bindings, types, values, sizes and section association -> OK");

    unsigned relocations = 0;
    for (const auto& section : object.sections()) {
        for (const auto& relocation : section.relocations()) {
            SmallString<32> type;
            relocation.getTypeName(type);
            auto symbol = relocation.getSymbol();
            if (symbol == object.symbol_end()) return fail("relocation symbol absent");
            auto name = symbol->getName();
            if (!ok(name)) return fail("relocation symbol name");
            auto addend = ELFRelocationRef(relocation).getAddend();
            if (!ok(addend)) return fail("relocation addend");
            if (relocation.getType() != ELF::R_AARCH64_ABS64 || type != "R_AARCH64_ABS64" ||
                relocation.getOffset() != 0 || *name != "sh9_missing" || *addend != 9)
                return fail("relocation type, target, offset or addend");
            ++relocations;
        }
    }
    if (relocations != 1) return fail("relocation count");
    record("SH9-d4: AArch64 relocation names, symbols, offsets and signed addends -> OK");

    {
        auto big = ObjectFile::createELFObjectFile(MemoryBufferRef(bytes(FixtureELF32BE), "armeb.o"));
        if (!ok(big)) return fail("ELF32BE creation");
        // Object reports EM_ARM independently of byte order; the ELF type carries it. — KESTREL
        if (!isa<ELF32BEObjectFile>(**big) || (*big)->getArch() != Triple::arm ||
            (*big)->getBytesInAddress() != 4) return fail("ELF32BE class, endian or architecture");
        bool value = false;
        for (const auto& section : (*big)->sections()) {
            auto name = section.getName();
            if (!ok(name)) return fail("ELF32BE section name");
            if (*name == ".data") {
                auto contents = section.getContents();
                if (!ok(contents)) return fail("ELF32BE data");
                value = contents->size() == 4 && support::endian::read32be(contents->data()) == 9;
            }
        }
        if (!value) return fail("ELF32BE data byte order");
    }
    record("SH9-d4: ELF32BE ARM identity and big-endian data decoding -> OK");

    {
        std::string bad = bytes(FixtureELF64LE).str();
        support::endian::write64le(bad.data() + 0x28, bad.size() + 4096);
        if (!rejectsELF(bytes(FixtureELF64LE).take_front(63)) || !rejectsELF(bad))
            return fail("truncated or out-of-range ELF accepted");
        auto names = ObjectFile::createELFObjectFile(MemoryBufferRef(bytes(FixtureBadSymbol), "bad-symbol.o"));
        bool refused = false;
        if (!names) refused = !toString(names.takeError()).empty();
        else for (const auto& symbol : (*names)->symbols()) {
            auto name = symbol.getName();
            if (!name) { refused = !toString(name.takeError()).empty(); break; }
        }
        if (!refused) return fail("invalid symbol string offset accepted");
    }
    record("SH9-d4: truncated ELF, section bounds and invalid symbol strings return errors -> OK");

    if (!archiveChecks(MemoryBufferRef(bytes(FixtureArchive), "objects.a"))) return fail("GNU archive");
    {
        auto shortArchive = Archive::create(MemoryBufferRef(bytes(FixtureArchive).take_front(30), "short.a"));
        bool refused = false;
        if (!shortArchive) refused = !toString(shortArchive.takeError()).empty();
        else {
            Error error = Error::success();
            for (const auto& child : (*shortArchive)->children(error)) (void)child;
            if (error) refused = !toString(std::move(error)).empty();
        }
        if (!refused) return fail("truncated archive accepted");
    }
    record("SH9-d4: GNU archive members, long names, symbol index and truncation errors -> OK");

    if (!fileChecks("/SH9D4.O", bytes(FixtureELF64LE), false) ||
        !fileChecks("/SH9D4.A", bytes(FixtureArchive), true)) return fail("file-backed object/archive");
    record("SH9-d4: file-backed ELF and archive parsing on the RAM volume -> OK");
    record("SH9-d4 LLVM Object green");
    return 0;
}
