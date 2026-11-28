//J952_r18_001
// Generic MC data emission needs no target instruction backend. — KESTREL
#include "llvm/BinaryFormat/Dwarf.h"
#include "llvm/BinaryFormat/ELF.h"
#include "llvm/DebugInfo/DWARF/LowLevel/DWARFCFIProgram.h"
#include "llvm/DebugInfo/DWARF/LowLevel/DWARFUnwindTable.h"
#include "llvm/MC/MCAsmBackend.h"
#include "llvm/MC/MCAsmInfoELF.h"
#include "llvm/MC/MCCodeEmitter.h"
#include "llvm/MC/MCContext.h"
#include "llvm/MC/MCExpr.h"
#include "llvm/MC/MCFixup.h"
#include "llvm/MC/MCInst.h"
#include "llvm/MC/MCInstPrinter.h"
#include "llvm/MC/MCInstrInfo.h"
#include "llvm/MC/MCRegisterInfo.h"
#include "llvm/MC/MCSectionELF.h"
#include "llvm/MC/MCStreamer.h"
#include "llvm/MC/MCSymbolELF.h"
#include "llvm/MC/MCValue.h"
#include "llvm/MC/StringTableBuilder.h"
#include "llvm/MC/TargetRegistry.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/ErrorHandling.h"
#include "llvm/Support/FormattedStream.h"
#include "llvm/Support/MemoryBuffer.h"
#include <cstdio>
#include <memory>

using namespace llvm;

class DataAsmInfo : public MCAsmInfoELF {
public:
    DataAsmInfo() { CodePointerSize = CalleeSaveStackSlotSize = 8; }
};

// MCAsmStreamer requires a printer. Fail closed if this data-only fixture ever
// reaches an instruction path; it provides no target instruction behavior. — KESTREL
class DataOnlyPrinter : public MCInstPrinter {
public:
    using MCInstPrinter::MCInstPrinter;
    std::pair<const char *, uint64_t> getMnemonic(const MCInst &) const override {
        report_fatal_error("instruction mnemonic requested in data-only proof");
    }
    void printInst(const MCInst *, uint64_t, StringRef, const MCSubtargetInfo &, raw_ostream &) override {
        report_fatal_error("instruction printing requested in data-only proof");
    }
};

static void record(StringRef text) { outs() << text << '\n'; outs().flush(); }
static int fail(const char* why) {
    outs() << "SH9-d5: " << why << "\nSH9-d5 LLVM MC FAILED\n";
    outs().flush();
    return 1;
}

extern "C" int main() {
    record("SH9-d5 LLVM MC begin");
    DataAsmInfo info;
    MCInstrInfo instructions{};
    MCRegisterInfo registers{};
    MCContext context(Triple("aarch64-unknown-kumo"), &info, &registers, nullptr);
    auto *section = context.getELFSection(".sh9.data", ELF::SHT_PROGBITS, ELF::SHF_ALLOC | ELF::SHF_WRITE);
    auto *symbol = context.getOrCreateSymbol("sh9_data");
    auto *elfSymbol = static_cast<MCSymbolELF *>(symbol);
    elfSymbol->setBinding(ELF::STB_GLOBAL);
    elfSymbol->setType(ELF::STT_OBJECT);
    if (symbol != context.getOrCreateSymbol("sh9_data") ||
        section != context.getELFSection(".sh9.data", ELF::SHT_PROGBITS, ELF::SHF_ALLOC | ELF::SHF_WRITE) ||
        section->getType() != ELF::SHT_PROGBITS || elfSymbol->getBinding() != ELF::STB_GLOBAL ||
        elfSymbol->getType() != ELF::STT_OBJECT || !symbol->isUndefined())
        return fail("ELF context, symbols or sections");
    record("SH9-d5: MC context, ELF section uniquing and symbol attributes -> OK");

    const auto *forty = MCConstantExpr::create(40, context);
    const auto *two = MCConstantExpr::create(2, context);
    const auto *sum = MCBinaryExpr::createAdd(forty, two, context);
    auto *alias = context.getOrCreateSymbol("sh9_alias");
    alias->setVariableValue(sum);
    int64_t value = 0;
    const auto *ref = MCSymbolRefExpr::create(symbol, context);
    const auto *relocExpr = MCBinaryExpr::createAdd(ref, two, context);
    MCValue reloc;
    if (!sum->evaluateAsAbsolute(value) || value != 42 ||
        !MCSymbolRefExpr::create(alias, context)->evaluateAsAbsolute(value) || value != 42 ||
        !MCUnaryExpr::createMinus(sum, context)->evaluateAsAbsolute(value) || value != -42 ||
        !relocExpr->evaluateAsRelocatable(reloc, nullptr) || reloc.getAddSym() != symbol ||
        reloc.getSubSym() || reloc.getConstant() != 2 || relocExpr->evaluateAsAbsolute(value) ||
        MCBinaryExpr::createDiv(two, MCConstantExpr::create(0, context), context)->evaluateAsAbsolute(value))
        return fail("expression evaluation or rejected invalid arithmetic");
    record("SH9-d5: absolute and alias expressions, relocation addends and invalid arithmetic -> OK");

    MCInst inst;
    inst.setOpcode(17);
    inst.addOperand(MCOperand::createImm(-42));
    inst.addOperand(MCOperand::createReg(7));
    inst.addOperand(MCOperand::createExpr(relocExpr));
    auto fixup = MCFixup::create(4, relocExpr, FK_Data_8);
    if (inst.getOpcode() != 17 || inst.getNumOperands() != 3 ||
        inst.getOperand(0).getImm() != -42 || inst.getOperand(1).getReg() != 7 ||
        inst.getOperand(2).getExpr() != relocExpr || fixup.getOffset() != 4 ||
        fixup.getKind() != FK_Data_8 || fixup.getValue() != relocExpr)
        return fail("instruction operands or data fixup representation");
    record("SH9-d5: instruction operands and data-fixup representation -> OK");

    std::string assembly;
    {
        raw_string_ostream text(assembly);
        auto formatted = std::make_unique<formatted_raw_ostream>(text);
        auto printer = std::make_unique<DataOnlyPrinter>(info, instructions, registers);
        std::unique_ptr<MCStreamer> stream(createAsmStreamer(context, std::move(formatted), std::move(printer), nullptr, nullptr));
        stream->switchSection(section);
        stream->emitSymbolAttribute(symbol, MCSA_Global);
        stream->emitLabel(symbol);
        stream->emitInt8(0x2a);
        stream->emitInt32(0x12345678);
        stream->emitBytes("KUMO");
        stream->emitValue(relocExpr, 8);
        stream->emitULEB128Value(MCConstantExpr::create(624485, context));
        stream->emitSLEB128Value(MCConstantExpr::create(-123456, context));
        stream->emitULEB128Value(relocExpr);
        stream->emitSLEB128Value(relocExpr);
        stream->emitValueToAlignment(Align(8));
        stream->finish();
    }
    StringRef emitted(assembly);
    if (!emitted.contains(".sh9.data") || !emitted.contains("sh9_data:") ||
        !emitted.contains("42") || !emitted.contains("305419896") || !emitted.contains("KUMO") ||
        !emitted.contains("sh9_data+2") || !emitted.contains(".uleb128 sh9_data+2") ||
        !emitted.contains(".sleb128 sh9_data+2") || !emitted.contains(".p2align\t3") ||
        symbol->isUndefined() || context.hadError()) {
        outs() << assembly;
        return fail("assembly data directives or label state");
    }
    record("SH9-d5: native assembly text labels, integers, bytes, expressions, LEB and alignment -> OK");

    StringTableBuilder strings(StringTableBuilder::ELF);
    strings.add("sh9_suffix"); strings.add("suffix"); strings.add("sh9_suffix");
    strings.finalize();
    std::string table;
    raw_string_ostream tableOut(table); strings.write(tableOut);
    auto longAt = strings.getOffset("sh9_suffix"), suffixAt = strings.getOffset("suffix");
    if (table.size() != strings.getSize() || table.front() != '\0' || suffixAt != longAt + 4 ||
        StringRef(table.data() + longAt) != "sh9_suffix" || StringRef(table.data() + suffixAt) != "suffix")
        return fail("ELF string-table suffix deduplication");
    StringTableBuilder coff(StringTableBuilder::WinCOFF);
    auto coffAt = coff.add("long_coff_symbol"); coff.finalizeInOrder();
    std::string coffBytes; raw_string_ostream coffOut(coffBytes); coff.write(coffOut);
    if (coffAt != 4 || coffBytes.size() != 21 || static_cast<unsigned char>(coffBytes[0]) != 21 ||
        StringRef(coffBytes.data() + coffAt) != "long_coff_symbol")
        return fail("COFF string-table size prefix");
    record("SH9-d5: ELF suffix deduplication and COFF string-table offsets and bytes -> OK");

    const uint8_t exprBytes[] = {dwarf::DW_OP_constu, 0xe5, 0x8e, 0x26, dwarf::DW_OP_plus_uconst, 7, dwarf::DW_OP_stack_value};
    DWARFExpression expression(DataExtractor(ArrayRef<uint8_t>(exprBytes), true, 8), 8);
    auto operation = expression.begin();
    if (operation->isError() || operation->getCode() != dwarf::DW_OP_constu || operation->getRawOperand(0) != 624485)
        return fail("DWARF expression ULEB operand");
    ++operation;
    if (operation->isError() || operation->getCode() != dwarf::DW_OP_plus_uconst || operation->getRawOperand(0) != 7)
        return fail("DWARF expression next operation");
    ++operation;
    if (operation->getCode() != dwarf::DW_OP_stack_value || ++operation != expression.end())
        return fail("DWARF expression end");
    const uint8_t unknownExpr[] = {0xff};
    DWARFExpression badExpression(DataExtractor(ArrayRef<uint8_t>(unknownExpr), true, 8), 8);
    if (!badExpression.begin()->isError()) return fail("unknown DWARF expression opcode accepted");

    const uint8_t cfiBytes[] = {dwarf::DW_CFA_def_cfa, 31, 16, uint8_t(dwarf::DW_CFA_offset | 30), 1,
                              uint8_t(dwarf::DW_CFA_advance_loc | 2), dwarf::DW_CFA_def_cfa_offset, 32};
    DWARFDataExtractorSimple data(ArrayRef<uint8_t>(cfiBytes), true, 8);
    dwarf::CFIProgram cfi(4, -8, Triple::aarch64);
    uint64_t offset = 0;
    if (auto error = cfi.parse(data, &offset, sizeof(cfiBytes))) { consumeError(std::move(error)); return fail("CFI parse"); }
    if (offset != sizeof(cfiBytes) || cfi.size() != 4) return fail("CFI boundaries");
    auto instruction = cfi.begin(); ++instruction;
    auto scaled = instruction->getOperandAsSigned(cfi, 1);
    if (!scaled) { consumeError(scaled.takeError()); return fail("CFI signed operand"); }
    if (*scaled != -8) return fail("CFI data alignment scaling");
    ++instruction;
    auto advance = instruction->getOperandAsUnsigned(cfi, 0);
    if (!advance) { consumeError(advance.takeError()); return fail("CFI unsigned operand"); }
    if (*advance != 8) return fail("CFI code alignment scaling");
    const uint8_t badCFI[] = {dwarf::DW_CFA_def_cfa, 0x80};
    DWARFDataExtractorSimple badData(ArrayRef<uint8_t>(badCFI), true, 8);
    dwarf::CFIProgram invalid(4, -8, Triple::aarch64); offset = 0;
    auto error = invalid.parse(badData, &offset, sizeof(badCFI));
    if (!error) return fail("truncated CFI accepted");
    if (toString(std::move(error)).empty()) return fail("CFI error text");
    dwarf::UnwindRow row; row.setAddress(0x1000);
    auto rows = dwarf::parseRows(cfi, row, nullptr);
    if (!rows) { consumeError(rows.takeError()); return fail("CFI unwind state machine"); }
    auto saved = row.getRegisterLocations().getRegisterLocation(30);
    if (rows->size() != 1 || rows->front().getAddress() != 0x1000 || row.getAddress() != 0x1008 ||
        row.getCFAValue().getRegister() != 31 || row.getCFAValue().getOffset() != 32 ||
        !saved || saved->getOffset() != -8 || !saved->getDereference())
        return fail("unwind rows, CFA or register save rule");
    record("SH9-d5: DWARF expressions, CFI decoding and scaling, unwind rows and truncated input -> OK");

    const char *path = "/SH9D5.S";
    std::error_code ec;
    { raw_fd_ostream file(path, ec, sys::fs::OF_None); if (ec) return fail("create assembly file");
      file << assembly; file.flush(); if (file.has_error()) return fail("write assembly file"); }
    auto read = MemoryBuffer::getFile(path);
    if (!read || (*read)->getBuffer() != assembly) return fail("read assembly file bytes");
    if (std::remove(path) != 0) return fail("remove assembly file");
    record("SH9-d5: emitted assembly file round trip on the RAM volume -> OK");
    record("SH9-d5 LLVM MC green");
    return 0;
}
