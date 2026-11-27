//J949_r15_001
// SH9-d2: the native BinaryFormat library's byte, table and metadata helpers. The temporary
// ELF fixture lives on the granted RAM FAT volume. Object parsing comes next. — KESTREL
#include "llvm/ADT/Twine.h"
#include "llvm/BinaryFormat/AMDGPUMetadataVerifier.h"
#include "llvm/BinaryFormat/COFF.h"
#include "llvm/BinaryFormat/DXContainer.h"
#include "llvm/BinaryFormat/Dwarf.h"
#include "llvm/BinaryFormat/ELF.h"
#include "llvm/BinaryFormat/Magic.h"
#include "llvm/BinaryFormat/MsgPackDocument.h"
#include "llvm/BinaryFormat/MsgPackReader.h"
#include "llvm/BinaryFormat/MsgPackWriter.h"
#include "llvm/BinaryFormat/SFrame.h"
#include "llvm/BinaryFormat/Wasm.h"
#include "llvm/BinaryFormat/XCOFF.h"
#include "llvm/Support/Endian.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/ScopedPrinter.h"
#include "llvm/Support/raw_ostream.h"

using namespace llvm;

static int fail(const char* why) {
    outs() << "SH9-d2: " << why << "\nSH9-d2 LLVM BinaryFormat FAILED\n";
    outs().flush();
    return 1;
}

static void record(StringRef text) {
    outs() << text << '\n';
    outs().flush();
}

extern "C" int main() {
    record("SH9-d2 LLVM BinaryFormat begin");

    // ELF magic distinguishes types in both byte orders; short and unknown data refuse.
    char elf[64] = {'\x7f', 'E', 'L', 'F', 2, 1, 1};
    support::endian::write16le(elf + 16, ELF::ET_REL);
    support::endian::write16le(elf + 18, ELF::EM_AARCH64);
    if (identify_magic(StringRef(elf, sizeof(elf))) != file_magic::elf_relocatable ||
        identify_magic(StringRef(elf, 17)) != file_magic::unknown ||
        identify_magic(StringRef("\x7f" "EL", 3)) != file_magic::unknown ||
        identify_magic("kumo") != file_magic::unknown)
        return fail("ELF magic or truncated input");
    char big[64];
    memcpy(big, elf, sizeof(big));
    big[5] = 2;
    support::endian::write16be(big + 16, ELF::ET_DYN);
    if (identify_magic(StringRef(big, sizeof(big))) != file_magic::elf_shared_object ||
        identify_magic("!<arch>\n") != file_magic::archive ||
        identify_magic(StringRef("\0asm\1\0\0\0", 8)) != file_magic::wasm_object ||
        identify_magic(StringRef("BC\xc0\xde", 4)) != file_magic::bitcode)
        return fail("ELF big endian, archive, Wasm or bitcode magic");
    char macho[32] = {'\xcf', '\xfa', '\xed', '\xfe'};
    support::endian::write32le(macho + 12, 1);
    if (identify_magic(StringRef(macho, sizeof(macho))) != file_magic::macho_object ||
        ELF::convertArchNameToEMachine("AARCH64") != ELF::EM_AARCH64 ||
        ELF::convertArchNameToEMachine("x86_64") != ELF::EM_X86_64 ||
        ELF::convertArchNameToEMachine("kumo-invalid") != ELF::EM_NONE ||
        ELF::convertTripleArchTypeToEMachine(Triple::aarch64) != ELF::EM_AARCH64)
        return fail("Mach-O magic or ELF architecture mapping");
    record("SH9-d2: magic, byte orders, truncation and ELF architecture mappings -> OK");

    // Exercise the filesystem overload through relibc and the existing volume grant.
    {
        const char* path = "/SH9D2.ELF";
        std::error_code ec;
        raw_fd_ostream stream(path, ec);
        if (ec) return fail("open ELF fixture");
        stream.write(elf, sizeof(elf));
        stream.close();
        if (stream.has_error()) return fail("write ELF fixture");
        file_magic kind;
        if (identify_magic(Twine(path), kind) || kind != file_magic::elf_relocatable)
            return fail("identify_magic on volume");
        auto buffer = MemoryBuffer::getFile(path);
        if (!buffer || (*buffer)->getBuffer() != StringRef(elf, sizeof(elf)))
            return fail("ELF fixture exact bytes");
        if (sys::fs::remove(path)) return fail("remove ELF fixture");
        if (identify_magic(Twine(path), kind) != std::errc::no_such_file_or_directory)
            return fail("missing file error");
    }
    record("SH9-d2: file magic and exact ELF bytes on the volume, missing-file error -> OK");

    {
        using namespace dwarf;
        FormParams v2{2, 8, DWARF32}, v5{5, 8, DWARF32}, wide{5, 8, DWARF64};
        if (TagString(DW_TAG_subprogram) != "DW_TAG_subprogram" ||
            getTag("DW_TAG_subprogram") != DW_TAG_subprogram ||
            getTag("DW_TAG_kumo_invalid") != DW_TAG_invalid || !TagString(0xffff).empty() ||
            OperationEncodingString(DW_OP_fbreg) != "DW_OP_fbreg" ||
            CallFrameString(DW_CFA_AARCH64_negate_ra_state, Triple::aarch64) != "DW_CFA_AARCH64_negate_ra_state" ||
            getFixedFormByteSize(DW_FORM_ref_addr, v2) != 8 ||
            getFixedFormByteSize(DW_FORM_ref_addr, v5) != 4 ||
            getFixedFormByteSize(DW_FORM_strp, wide) != 8 ||
            getFixedFormByteSize(DW_FORM_udata, v5).has_value() ||
            isValidFormForVersion(DW_FORM_addrx, 4, false) ||
            !isValidFormForVersion(DW_FORM_addrx, 5, false))
            return fail("DWARF names, forms or versions");
    }
    record("SH9-d2: DWARF names, AArch64 CFI, form sizes and version gates -> OK");

    {
        char name[8] = {};
        if (!COFF::encodeSectionName(name, 9999999) || StringRef(name, 8) != "/9999999" ||
            !COFF::encodeSectionName(name, 0xfffffffffULL) || StringRef(name, 8) != "////////" ||
            COFF::encodeSectionName(name, 0x1000000000ULL))
            return fail("COFF section-name boundaries");
        if (wasm::sectionTypeToString(wasm::WASM_SEC_CODE) != "CODE" ||
            wasm::relocTypetoString(wasm::R_WASM_MEMORY_ADDR_I32) != "R_WASM_MEMORY_ADDR_I32" ||
            !wasm::relocTypeHasAddend(wasm::R_WASM_MEMORY_ADDR_I32) ||
            wasm::relocTypeHasAddend(wasm::R_WASM_FUNCTION_INDEX_LEB) ||
            XCOFF::getMappingClassString(XCOFF::XMC_PR) != "PR" ||
            XCOFF::getRelocationTypeString(XCOFF::R_POS) != "R_POS" ||
            dxbc::parsePartType("DXIL") != dxbc::PartType::DXIL ||
            dxbc::parsePartType("KUMO") != dxbc::PartType::Unknown ||
            !dxbc::isValidParameterType(0) || dxbc::isValidParameterType(0xffffffff))
            return fail("Wasm, XCOFF or DXContainer tables");
        auto abis = sframe::getABIs();
        if (abis.size() != 3 || abis[1].Name != "AArch64EndianLittle" ||
            abis[1].Value != sframe::ABI::AArch64EndianLittle)
            return fail("SFrame ABI table");
    }
    record("SH9-d2: COFF boundaries, Wasm relocations, XCOFF, DXContainer and SFrame -> OK");

    {
        std::string bytes;
        raw_string_ostream stream(bytes);
        msgpack::Writer writer(stream);
        writer.writeMapSize(2);
        writer.write(StringRef("n"));
        writer.write(int64_t(-33));
        writer.write(StringRef("s"));
        writer.write(StringRef("kumo"));
        const char expected[] = "\x82\xa1n\xd0\xdf\xa1s\xa4kumo";
        if (bytes != StringRef(expected, sizeof(expected) - 1)) return fail("MessagePack wire bytes");
        msgpack::Document doc;
        if (!doc.readFromBlob(bytes, false) || doc.getRoot().getMap()["n"].getInt() != -33 ||
            doc.getRoot().getMap()["s"].getString() != "kumo")
            return fail("MessagePack document read");
        std::string packed;
        doc.writeToBlob(packed);
        if (packed != bytes) return fail("MessagePack document round trip");
        std::string yaml;
        raw_string_ostream yamlStream(yaml);
        doc.toYAML(yamlStream);
        msgpack::Document fromYaml;
        if (!fromYaml.fromYAML(yaml) || fromYaml.getRoot().getMap()["s"].getString() != "kumo")
            return fail("MessagePack YAML round trip");
        msgpack::Reader reader(StringRef("\xd9\x04" "ab", 4));
        msgpack::Object object;
        auto truncated = reader.read(object);
        if (truncated) return fail("MessagePack truncated string accepted");
        if (toString(truncated.takeError()).empty()) return fail("MessagePack missing error text");
        msgpack::Reader empty{StringRef()};
        auto eof = empty.read(object);
        if (!eof) { consumeError(eof.takeError()); return fail("MessagePack EOF error"); }
        if (*eof) return fail("MessagePack EOF produced an object");
    }
    record("SH9-d2: MessagePack exact bytes, document/YAML round trips and truncated input -> OK");

    {
        msgpack::Document doc;
        auto& root = doc.getRoot().getMap(true);
        auto version = doc.getArrayNode();
        version.push_back(doc.getNode(1u));
        version.push_back(doc.getNode(0u));
        root["amdhsa.version"] = version;
        root["amdhsa.kernels"] = doc.getArrayNode();
        AMDGPU::HSAMD::V3::MetadataVerifier verifier(true);
        if (!verifier.verify(doc.getRoot())) return fail("typed AMDGPU metadata refused");
        root["amdhsa.version"] = doc.getNode("wrong type");
        if (verifier.verify(doc.getRoot())) return fail("ill-typed AMDGPU metadata accepted");
    }
    record("SH9-d2: AMDGPU metadata accepts typed fields and rejects invalid types -> OK");
    record("SH9-d2 LLVM BinaryFormat green");
    return 0;
}
