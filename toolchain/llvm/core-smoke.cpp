//J958_r24_001
// SH9-d6: native IR construction and verification, with real Core/Remarks/BitstreamReader.
#include "llvm/BinaryFormat/Dwarf.h"
#include "llvm/IR/Constants.h"
#include "llvm/IR/DIBuilder.h"
#include "llvm/IR/Dominators.h"
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/Intrinsics.h"
#include "llvm/IR/LLVMContext.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/Verifier.h"
#include "llvm/Remarks/Remark.h"
#include "llvm/Remarks/RemarkParser.h"
#include "llvm/Remarks/RemarkSerializer.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/raw_ostream.h"

using namespace llvm;

static int fail(StringRef why) {
    outs() << "SH9-d6: " << why << "\nSH9-d6 LLVM Core FAILED\n";
    outs().flush();
    return 1;
}
static void record(StringRef text) { outs() << text << '\n'; outs().flush(); }

static bool remarksRoundTrip(remarks::Format format) {
    remarks::Remark input;
    input.RemarkType = remarks::Type::Passed;
    input.PassName = "sh9-d6";
    input.RemarkName = "BuiltIR";
    input.FunctionName = "maximum";
    input.Loc = remarks::RemarkLocation{"/CORE.C", 17, 3};
    input.Hotness = 42;
    input.Args.emplace_back("Count", "2");
    std::string encoded;
    raw_string_ostream out(encoded);
    auto serializer = remarks::createRemarkSerializer(format, out);
    if (!serializer) { consumeError(serializer.takeError()); return false; }
    (*serializer)->emit(input);
    (*serializer)->finalize();
    serializer->reset();
    auto parser = remarks::createRemarkParser(format, encoded);
    if (!parser) { consumeError(parser.takeError()); return false; }
    auto result = (*parser)->next();
    if (!result) { consumeError(result.takeError()); return false; }
    if (**result != input || (*result)->Args[0].getValAsInt<unsigned>() != 2) return false;
    auto eof = (*parser)->next();
    if (eof) return false;
    auto error = eof.takeError();
    bool ended = error.isA<remarks::EndOfFileError>();
    consumeError(std::move(error));
    return ended;
}

extern "C" int main() {
    record("SH9-d6 LLVM Core begin");
    LLVMContext context;
    Module module("sh9-d6", context);
    module.setTargetTriple(Triple("aarch64-unknown-kumo"));
    module.setDataLayout("e-m:e-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32");
    auto *i32 = Type::getInt32Ty(context);
    auto *aggregate = StructType::get(context, {Type::getInt8Ty(context), i32, Type::getInt64Ty(context)});
    const auto &layout = module.getDataLayout();
    const auto *structure = layout.getStructLayout(aggregate);
    if (structure->getElementOffset(1) != 4 || structure->getElementOffset(2) != 8 ||
        layout.getTypeAllocSize(aggregate) != 16 || layout.getPointerSize() != 8 ||
        module.getTargetTriple().getOS() != Triple::Kumo) return fail("target data layout");
    record("SH9-d6: KUMO module, types and AArch64 aggregate layout -> OK");

    auto *function = Function::Create(FunctionType::get(i32, {i32, i32}, false),
                                      GlobalValue::ExternalLinkage, "maximum", module);
    function->getArg(0)->setName("left");
    function->getArg(1)->setName("right");
    auto *entry = BasicBlock::Create(context, "entry", function);
    auto *yes = BasicBlock::Create(context, "yes", function);
    auto *no = BasicBlock::Create(context, "no", function);
    auto *merge = BasicBlock::Create(context, "merge", function);
    IRBuilder<> builder(entry);
    auto *cmp = builder.CreateICmpSGT(function->getArg(0), function->getArg(1), "larger");
    builder.CreateCondBr(cmp, yes, no);
    builder.SetInsertPoint(yes); builder.CreateBr(merge);
    builder.SetInsertPoint(no); builder.CreateBr(merge);
    builder.SetInsertPoint(merge);
    auto *phi = builder.CreatePHI(i32, 2, "chosen");
    phi->addIncoming(function->getArg(0), yes);
    phi->addIncoming(function->getArg(1), no);
    auto *ret = builder.CreateRet(phi);
    std::string diagnostics;
    raw_string_ostream diag(diagnostics);
    if (verifyModule(module, &diag)) return fail("valid SSA rejected");
    DominatorTree dominators(*function);
    if (!dominators.dominates(entry, merge) || dominators.dominates(yes, merge) ||
        dominators.findNearestCommonDominator(yes, no) != entry || phi->getNumIncomingValues() != 2)
        return fail("control flow and dominance");
    record("SH9-d6: branching SSA, PHI predecessors and dominator tree -> OK");

    builder.SetInsertPoint(ret);
    auto *folded = dyn_cast<ConstantInt>(builder.CreateAdd(builder.getInt32(19), builder.getInt32(23)));
    if (!folded || folded->getZExtValue() != 42) return fail("constant folding");
    phi->replaceAllUsesWith(function->getArg(0));
    if (!phi->use_empty() || ret->getReturnValue() != function->getArg(0)) return fail("replace uses");
    ret->setOperand(0, phi);
    if (!phi->hasOneUse() || verifyModule(module, &diag)) return fail("restore uses");
    function->addFnAttr(Attribute::NoUnwind);
    auto *bswap = Intrinsic::getOrInsertDeclaration(&module, Intrinsic::bswap, {i32});
    if (bswap->getName() != "llvm.bswap.i32" || bswap->getIntrinsicID() != Intrinsic::bswap ||
        !function->hasFnAttribute(Attribute::NoUnwind)) return fail("intrinsic tables or attributes");
    record("SH9-d6: constant folding, use replacement, attributes and generated intrinsics -> OK");

    phi->removeIncomingValue(no, false);
    diagnostics.clear();
    if (!verifyFunction(*function, &diag) || diagnostics.find("PHINode") == std::string::npos)
        return fail("invalid PHI accepted or undiagnosed");
    phi->addIncoming(function->getArg(1), no);
    auto *bad = Function::Create(FunctionType::get(i32, false), GlobalValue::ExternalLinkage, "bad", module);
    builder.SetInsertPoint(BasicBlock::Create(context, "entry", bad));
    builder.CreateRetVoid();
    diagnostics.clear();
    if (!verifyFunction(*bad, &diag) || diagnostics.find("return") == std::string::npos)
        return fail("invalid return accepted or undiagnosed");
    bad->eraseFromParent();
    if (verifyModule(module, &diag)) return fail("repaired module rejected");
    record("SH9-d6: verifier rejects missing PHI edges and wrong return types, repairs verify -> OK");

    module.getOrInsertNamedMetadata("kumo.proof")->addOperand(
        MDNode::get(context, {MDString::get(context, "native IR")}));
    DIBuilder debug(module);
    auto *file = debug.createFile("CORE.C", "/");
    debug.createCompileUnit(dwarf::DW_LANG_C, file, "KUMO SH9-d6", false, "", 0);
    auto *debugInt = debug.createBasicType("int", 32, dwarf::DW_ATE_signed);
    Metadata *parameters[] = {debugInt, debugInt, debugInt};
    auto *signature = debug.createSubroutineType(debug.getOrCreateTypeArray(parameters));
    auto *subprogram = debug.createFunction(file, "maximum", "maximum", file, 1, signature, 1,
                                            DINode::FlagPrototyped, DISubprogram::SPFlagDefinition);
    function->setSubprogram(subprogram);
    ret->setDebugLoc(DILocation::get(context, 17, 3, subprogram));
    debug.finalize();
    module.addModuleFlag(Module::Warning, "Debug Info Version", DEBUG_METADATA_VERSION);
    if (verifyModule(module, &diag) || module.getNamedMetadata("llvm.dbg.cu")->getNumOperands() != 1 ||
        ret->getDebugLoc().getLine() != 17) return fail("debug metadata");
    record("SH9-d6: named metadata and finalized DWARF compile unit, types and locations -> OK");

    if (!remarksRoundTrip(remarks::Format::YAML) || !remarksRoundTrip(remarks::Format::Bitstream))
        return fail("remark round trip or EOF");
    auto invalid = remarks::createRemarkParser(remarks::Format::Bitstream, "not a bitstream");
    // The factory is lazy: parsing and its errors begin with next(). — TEAL
    if (invalid) {
        auto result = (*invalid)->next();
        if (result) return fail("invalid bitstream accepted");
        auto error = result.takeError();
        bool malformed = !error.isA<remarks::EndOfFileError>();
        consumeError(std::move(error));
        if (!malformed) return fail("invalid bitstream treated as EOF");
    } else {
        consumeError(invalid.takeError());
    }
    record("SH9-d6: YAML and bitstream remarks round trip, EOF and invalid input -> OK");

    std::string text;
    raw_string_ostream printed(text);
    module.print(printed, nullptr);
    if (text.find("define i32 @maximum") == std::string::npos || text.find("phi i32") == std::string::npos ||
        text.find("!DICompileUnit") == std::string::npos || text.find("!kumo.proof") == std::string::npos)
        return fail("IR printing");
    const char *path = "/SH9D6.LL";
    std::error_code error;
    {
        raw_fd_ostream output(path, error, sys::fs::OF_None);
        if (error) return fail("IR output open");
        output << text;
        output.flush();
        if (output.has_error()) { output.clear_error(); return fail("IR output write"); }
    }
    auto read = MemoryBuffer::getFile(path);
    if (!read || (*read)->getBuffer() != text || sys::fs::remove(path) || sys::fs::exists(path))
        return fail("IR file round trip");
    record("SH9-d6: printed IR exact file round trip on the RAM volume and cleanup -> OK");
    record("SH9-d6 LLVM Core green");
    return 0;
}
