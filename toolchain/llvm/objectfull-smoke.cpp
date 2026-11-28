//J960_r26_001
// Complete Object linkage: real generic readers and native indexed archive output.
#include "format-fixtures.h"
#include "ir-fixtures.h"
#include "object-fixtures.h"
#include "llvm/IR/LLVMContext.h"
#include "llvm/IR/Module.h"
#include "llvm/MC/MCAsmInfoELF.h"
#include "llvm/MC/MCParser/AsmLexer.h"
#include "llvm/Object/Archive.h"
#include "llvm/Object/ArchiveWriter.h"
#include "llvm/Object/Binary.h"
#include "llvm/Object/IRObjectFile.h"
#include "llvm/Object/ObjectFile.h"
#include "llvm/Object/TapiFile.h"
#include "llvm/Object/TapiUniversal.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Memory.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/TextAPI/InterfaceFile.h"
#include "llvm/TextAPI/TextAPIReader.h"
#include "llvm/TextAPI/TextAPIWriter.h"

using namespace llvm;
using namespace llvm::object;

template <size_t N> static StringRef bytes(const unsigned char (&input)[N]) {
    return StringRef(reinterpret_cast<const char *>(input), N);
}
static void record(StringRef text) { outs() << text << '\n'; outs().flush(); }
static int fail(StringRef why) {
    outs() << "SH9-d8: " << why << "\nSH9-d8 LLVM Object closure FAILED\n";
    outs().flush();
    return 1;
}
template <typename T> static bool good(Expected<T> &value) {
    if (value) return true;
    errs() << toString(value.takeError()) << '\n';
    return false;
}
static bool noError(Error error) {
    if (!error) return true;
    errs() << toString(std::move(error)) << '\n';
    return false;
}
class LexerInfo : public MCAsmInfoELF {
public:
    LexerInfo() { CodePointerSize = CalleeSaveStackSlotSize = 8; CommentString = "#"; }
};
static bool lexerChecks() {
    LexerInfo info;
    AsmLexer lexer(info);
    lexer.setBuffer("name: .long 0x2a, \"KUMO\" # comment\n");
    const AsmToken::TokenKind kinds[] = {AsmToken::Identifier, AsmToken::Colon, AsmToken::Identifier,
        AsmToken::Integer, AsmToken::Comma, AsmToken::String, AsmToken::EndOfStatement, AsmToken::Eof};
    unsigned index = 0;
    for (auto kind : kinds) {
        const auto &token = lexer.Lex();
        if (token.getKind() != kind) return false;
        if (index == 0 && token.getString() != "name") return false;
        if (index == 2 && token.getString() != ".long") return false;
        if (index == 3 && token.getIntVal() != 42) return false;
        if (index == 5 && token.getStringContents() != "KUMO") return false;
        ++index;
    }
    lexer.setBuffer("\"unterminated");
    return lexer.Lex().is(AsmToken::Error) && !lexer.getErr().empty();
}
static bool irSymbols(SymbolicFile &file) {
    bool answer = false, choose = false, helper = false;
    for (const auto &symbol : file.symbols()) {
        std::string name;
        raw_string_ostream output(name);
        if (!noError(symbol.printName(output))) return false;
        auto flags = symbol.getFlags();
        if (!good(flags)) return false;
        if (name == "answer" || name == "choose" || name == "helper") {
            if (*flags & SymbolRef::SF_Undefined) return false;
            answer |= name == "answer"; choose |= name == "choose"; helper |= name == "helper";
        }
    }
    return answer && choose && helper;
}
static constexpr char Stub[] = R"(--- !tapi-tbd
tbd-version: 4
targets: [ arm64-macos ]
install-name: '/usr/lib/libSH9.dylib'
current-version: 1.2.3
compatibility-version: 1.0
exports:
  - targets: [ arm64-macos ]
    symbols: [ _sh9_export ]
    weak-symbols: [ _sh9_weak ]
...
)";
static bool interfaceMatches(const MachO::InterfaceFile &file) {
    if (file.getInstallName() != "/usr/lib/libSH9.dylib" || file.symbolsCount() != 2) return false;
    bool exported = false, weak = false;
    for (const auto *symbol : file.symbols()) {
        if (!symbol->hasArchitecture(MachO::AK_arm64)) return false;
        exported |= symbol->getName() == "_sh9_export" && !symbol->isWeakDefined();
        weak |= symbol->getName() == "_sh9_weak" && symbol->isWeakDefined();
    }
    return exported && weak;
}
static bool textChecks() {
    auto file = MachO::TextAPIReader::get(MemoryBufferRef(Stub, "input.tbd"));
    if (!good(file) || !interfaceMatches(**file)) return false;
    for (auto kind : {MachO::FileType::TBD_V4, MachO::FileType::TBD_V5}) {
        std::string encoded;
        raw_string_ostream output(encoded);
        if (!noError(MachO::TextAPIWriter::writeToStream(output, **file, kind))) return false;
        auto parsed = MachO::TextAPIReader::get(MemoryBufferRef(encoded, "roundtrip.tbd"));
        if (!good(parsed) || (*parsed)->getFileType() != kind || !interfaceMatches(**parsed)) return false;
    }
    auto binary = createBinary(MemoryBufferRef(Stub, "input.tbd"));
    if (!good(binary) || !(*binary)->isTapiUniversal()) return false;
    const auto *universal = cast<TapiUniversal>(binary->get());
    if (universal->getNumberOfObjects() != 1) return false;
    for (const auto &slice : universal->objects()) {
        if (slice.getArchFlagName() != "arm64" || slice.getInstallName() != "/usr/lib/libSH9.dylib") return false;
        auto object = slice.getAsObjectFile();
        if (!good(object)) return false;
        unsigned count = 0;
        for (const auto &symbol : (*object)->symbols()) {
            auto flags = symbol.getFlags();
            if (!good(flags) || (*flags & SymbolRef::SF_Undefined)) return false;
            ++count;
        }
        if (count != 2) return false;
    }
    auto invalid = MachO::TextAPIReader::get(MemoryBufferRef("not a text stub", "bad.tbd"));
    if (invalid) return false;
    return !toString(invalid.takeError()).empty();
}
static bool archiveChecks(Archive &archive, LLVMContext &context) {
    if (archive.isThin() || archive.kind() != Archive::K_GNU || !archive.hasSymbolTable()) return false;
    Error error = Error::success();
    unsigned count = 0;
    for (const auto &child : archive.children(error)) {
        auto name = child.getName();
        auto buffer = child.getMemoryBufferRef();
        if (!good(name) || !good(buffer)) return false;
        auto binary = createBinary(*buffer, &context);
        if (!good(binary)) return false;
        if (count == 0 && (*name != "native-input.o" || !(*binary)->isELF() ||
                           buffer->getBuffer() != bytes(FixtureELF64LE))) return false;
        if (count == 1 && (*name != "long_bitcode_member_name.bc" || !(*binary)->isIR() ||
                           buffer->getBuffer() != bytes(FixtureBitcode))) return false;
        ++count;
    }
    if (!noError(std::move(error)) || count != 2) return false;
    for (StringRef name : {StringRef("sh9_answer"), StringRef("choose"), StringRef("helper"), StringRef("answer")}) {
        auto found = archive.findSym(name);
        if (!good(found) || !*found) return false;
    }
    auto absent = archive.findSym("sh9_missing");
    return good(absent) && !*absent;
}

extern "C" int main() {
    record("SH9-d8 LLVM Object closure begin");
    alignas(256) unsigned char privateBytes[768];
    for (unsigned i = 0; i < sizeof(privateBytes); ++i) privateBytes[i] = static_cast<unsigned char>(i);
    sys::Memory::InvalidateInstructionCache(privateBytes + 3, sizeof(privateBytes) - 7);
    sys::Memory::InvalidateInstructionCache(privateBytes + 3, 0);
    for (unsigned i = 0; i < sizeof(privateBytes); ++i)
        if (privateBytes[i] != static_cast<unsigned char>(i)) return fail("cache operation changed data");
    record("SH9-d8: compiler-rt cache builtin runs through LLVM on an unaligned private range -> OK");
    if (!lexerChecks()) return fail("MC lexer tokens or diagnostic");
    record("SH9-d8: MC lexer identifiers, directives, integers, strings and unterminated-string error -> OK");

    struct Input { StringRef data; Triple::ObjectFormatType format; Triple::ArchType architecture; };
    const Input inputs[] = {{bytes(FixtureELF64LE), Triple::ELF, Triple::aarch64},
        {bytes(FixtureELF32BE), Triple::ELF, Triple::arm},
        {bytes(FixtureCOFF), Triple::COFF, Triple::aarch64},
        {bytes(FixtureMachO), Triple::MachO, Triple::aarch64},
        {bytes(FixtureWasm), Triple::Wasm, Triple::wasm32}};
    for (const auto &input : inputs) {
        auto binary = createBinary(MemoryBufferRef(input.data, "host-input.o"));
        if (!good(binary) || !isa<ObjectFile>(binary->get())) return fail("generic object factory");
        auto &object = *cast<ObjectFile>(binary->get());
        bool format = input.format == Triple::Wasm ? object.isWasm() : object.getTripleObjectFormat() == input.format;
        if (!format || object.getArch() != input.architecture) return fail("generic object identity");
    }
    record("SH9-d8: generic binary factory reads ELF64LE, ELF32BE, COFF, Mach-O and Wasm -> OK");

    LLVMContext context;
    auto ir = createBinary(MemoryBufferRef(bytes(FixtureBitcode), "input.bc"), &context);
    if (!good(ir) || !isa<IRObjectFile>(ir->get()) || !irSymbols(*cast<IRObjectFile>(ir->get())) ||
        cast<IRObjectFile>(ir->get())->getTargetTriple() != "aarch64-unknown-kumo") return fail("IR symbolic object");
    record("SH9-d8: generic bitcode factory exposes KUMO IR object symbols and definitions -> OK");

    if (!textChecks()) return fail("TextAPI or Object TAPI adapter");
    record("SH9-d8: TextAPI YAML v4 and JSON v5 round trips, weak exports, TAPI object adapter and invalid input -> OK");

    SmallVector<NewArchiveMember, 2> members;
    members.emplace_back(MemoryBufferRef(bytes(FixtureELF64LE), "native-input.o"));
    members.back().MemberName = "native-input.o";
    members.emplace_back(MemoryBufferRef(bytes(FixtureBitcode), "long_bitcode_member_name.bc"));
    members.back().MemberName = "long_bitcode_member_name.bc";
    auto encoded = writeArchiveToBuffer(members, SymtabWritingMode::NormalSymtab, Archive::K_GNU, true, false);
    if (!good(encoded)) return fail("native archive buffer writer");
    auto repeated = writeArchiveToBuffer(members, SymtabWritingMode::NormalSymtab, Archive::K_GNU, true, false);
    if (!good(repeated) || (*encoded)->getBuffer() != (*repeated)->getBuffer()) return fail("archive determinism");
    auto archive = createBinary((*encoded)->getMemBufferRef(), &context);
    if (!good(archive) || !isa<Archive>(archive->get()) || !archiveChecks(*cast<Archive>(archive->get()), context))
        return fail("native archive index or members");
    record("SH9-d8: native GNU archive bytes repeat exactly, mixed ELF/bitcode members and symbol index read back -> OK");

    for (const auto &input : inputs) {
        auto invalid = createBinary(MemoryBufferRef(input.data.take_front(4), "truncated.o"));
        if (invalid) return fail("truncated object accepted");
        if (toString(invalid.takeError()).empty()) return fail("missing object error diagnostic");
    }
    auto unknown = createBinary(MemoryBufferRef("not a binary", "unknown"));
    if (unknown) return fail("unknown binary accepted");
    consumeError(unknown.takeError());
    record("SH9-d8: generic factories reject truncated ELF, COFF, Mach-O, Wasm and unknown magic -> OK");

    const char *path = "/SH9D8.A";
    std::error_code error;
    {
        raw_fd_ostream output(path, error, sys::fs::OF_None);
        if (error || !noError(writeArchiveToStream(output, members, SymtabWritingMode::NormalSymtab,
                                                  Archive::K_GNU, true, false))) return fail("archive file stream");
        output.flush();
        if (output.has_error()) { output.clear_error(); return fail("archive file write"); }
    }
    auto read = MemoryBuffer::getFile(path);
    auto owned = createBinary(path, &context);
    if (!read || (*read)->getBuffer() != (*encoded)->getBuffer() || !good(owned) ||
        !isa<Archive>(owned->getBinary()) || !archiveChecks(*cast<Archive>(owned->getBinary()), context))
        return fail("native archive file round trip");
    auto missing = createBinary("/NOFILE.A", &context);
    if (missing) return fail("missing archive file accepted");
    consumeError(missing.takeError());
    if (sys::fs::remove(path) || sys::fs::exists(path)) return fail("archive cleanup");
    record("SH9-d8: native archive stream saved and loaded through file factory, missing file refused and cleanup -> OK");
    record("SH9-d8 LLVM Object closure green");
    return 0;
}
