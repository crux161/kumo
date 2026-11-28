//J959_r25_001
// Native text/bitcode readers and the real upstream llvm-dis as a granted child.
#include "ir-fixtures.h"
#include "llvm/AsmParser/Parser.h"
#include "llvm/Bitcode/BitcodeReader.h"
#include "llvm/IR/Constants.h"
#include "llvm/IR/Instructions.h"
#include "llvm/IR/IntrinsicInst.h"
#include "llvm/IR/LLVMContext.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/Verifier.h"
#include "llvm/IRReader/IRReader.h"
#include "llvm/Support/FileSystem.h"
#include "llvm/Support/MemoryBuffer.h"
#include "llvm/Support/Program.h"
#include "llvm/Support/SourceMgr.h"
#include "llvm/Support/raw_ostream.h"
#include <cstdlib>

using namespace llvm;

static int fail(StringRef why) {
    outs() << "SH9-d7: " << why << "\nSH9-d7 LLVM IR readers FAILED\n";
    outs().flush();
    return 1;
}
static void record(StringRef text) { outs() << text << '\n'; outs().flush(); }
static StringRef textInput() {
    return StringRef(reinterpret_cast<const char *>(FixtureIR), sizeof(FixtureIR) - 1);
}
static MemoryBufferRef bitcodeInput() {
    return MemoryBufferRef(StringRef(reinterpret_cast<const char *>(FixtureBitcode), sizeof(FixtureBitcode)), "input.bc");
}
static bool matches(Module &module) {
    if (verifyModule(module, &errs()) || module.getTargetTriple().getOS() != Triple::Kumo ||
        module.getDataLayout().getPointerSize() != 8) return false;
    auto *value = module.getGlobalVariable("answer");
    auto *answer = value ? dyn_cast<ConstantInt>(value->getInitializer()) : nullptr;
    auto *choose = module.getFunction("choose");
    auto *helper = module.getFunction("helper");
    auto *bswap = module.getFunction("llvm.bswap.i32");
    if (!answer || answer->getZExtValue() != 42 || !choose || choose->size() != 4 ||
        choose->arg_size() != 2 || !choose->hasFnAttribute(Attribute::NoUnwind) || !helper || helper->empty() ||
        !bswap || bswap->getIntrinsicID() != Intrinsic::bswap)
        return false;
    auto *phi = dyn_cast<PHINode>(&choose->back().front());
    auto *ret = dyn_cast<ReturnInst>(choose->back().getTerminator());
    auto *named = module.getNamedMetadata("kumo.proof");
    return phi && phi->getNumIncomingValues() == 2 && ret && ret->getReturnValue() == phi &&
           ret->getDebugLoc().getLine() == 17 && ret->getDebugLoc().getCol() == 3 &&
           choose->getSubprogram() && module.getNamedMetadata("llvm.dbg.cu") &&
           named && named->getNumOperands() == 1 &&
           cast<MDString>(named->getOperand(0)->getOperand(0))->getString() == "native IR reader";
}
static bool writeFile(StringRef path, StringRef data) {
    std::error_code error;
    raw_fd_ostream file(path, error, sys::fs::OF_None);
    if (error) return false;
    file << data;
    file.flush();
    if (file.has_error()) { file.clear_error(); return false; }
    return true;
}
static bool readContains(StringRef path, StringRef text) {
    auto file = MemoryBuffer::getFile(path);
    return file && (*file)->getBuffer().contains(text);
}
static bool runDis(ArrayRef<StringRef> arguments, std::optional<StringRef> output, int expected) {
    const std::optional<StringRef> redirects[] = {StringRef(""), output, StringRef("/DIS.ERR")};
    std::string error;
    bool failed = false;
    int result = sys::ExecuteAndWait("/DIS.ELF", arguments, std::nullopt, redirects, 0, 0, &error, &failed);
    if (failed || result != expected) {
        errs() << "llvm-dis result=" << result << " failed=" << failed << " detail=" << error << '\n';
        return false;
    }
    return true;
}

extern "C" int main() {
    record("SH9-d7 LLVM IR readers begin");
    LLVMContext context;
    SMDiagnostic diagnostic;
    auto parsed = parseAssemblyString(textInput(), diagnostic, context);
    if (!parsed || !matches(*parsed)) return fail("text IR semantics");
    record("SH9-d7: textual IR parses SSA, globals, attributes, intrinsics and debug metadata -> OK");

    std::string printed;
    raw_string_ostream out(printed);
    parsed->print(out, nullptr);
    LLVMContext roundTripContext;
    auto roundTrip = parseAssemblyString(printed, diagnostic, roundTripContext);
    if (!roundTrip || !matches(*roundTrip)) return fail("printed IR reparse");
    for (StringRef broken : {StringRef("define i32 @bad() { entry: mystery i32 1 }"),
                             StringRef("define i32 @bad() { entry: ret i32 %missing }")}) {
        LLVMContext badContext;
        SMDiagnostic error;
        if (parseAssemblyString(broken, error, badContext) || error.getLineNo() < 1 || error.getMessage().empty())
            return fail("invalid text accepted or undiagnosed");
    }
    record("SH9-d7: printed IR reparses, malformed syntax and undefined values report diagnostics -> OK");

    LLVMContext eagerContext;
    auto eager = parseBitcodeFile(bitcodeInput(), eagerContext);
    if (!eager) { consumeError(eager.takeError()); return fail("eager bitcode parse"); }
    if (!matches(**eager)) return fail("bitcode semantics");
    record("SH9-d7: host LLVM 22 bitcode reads eagerly with matching IR and debug semantics -> OK");

    LLVMContext lazyContext;
    auto lazy = getLazyBitcodeModule(bitcodeInput(), lazyContext, true);
    if (!lazy) { consumeError(lazy.takeError()); return fail("lazy bitcode parse"); }
    auto *choose = (*lazy)->getFunction("choose");
    if (!choose || !choose->isMaterializable() || !choose->empty()) return fail("function was not lazy");
    if (auto error = (*lazy)->materializeMetadata()) { consumeError(std::move(error)); return fail("metadata materialization"); }
    if (auto error = choose->materialize()) { consumeError(std::move(error)); return fail("function materialization"); }
    if (choose->empty() || choose->isMaterializable()) return fail("function still lazy");
    if (auto error = (*lazy)->materializeAll()) { consumeError(std::move(error)); return fail("module materialization"); }
    if (!matches(**lazy)) return fail("materialized bitcode semantics");
    for (StringRef broken : {StringRef("not-bitcode"), bitcodeInput().getBuffer().take_front(20),
                             bitcodeInput().getBuffer().take_front(sizeof(FixtureBitcode) / 2)}) {
        LLVMContext badContext;
        auto invalid = parseBitcodeFile(MemoryBufferRef(broken, "truncated.bc"), badContext);
        if (invalid) return fail("invalid bitcode accepted");
        if (toString(invalid.takeError()).empty()) return fail("bitcode error lacked detail");
    }
    record("SH9-d7: lazy functions and metadata materialize, bad magic and truncated bitcode return errors -> OK");

    for (MemoryBufferRef input : {MemoryBufferRef(textInput(), "text.ll"), bitcodeInput()}) {
        LLVMContext dispatchContext;
        auto module = parseIR(input, diagnostic, dispatchContext);
        if (!module || !matches(*module)) return fail("IRReader in-memory format dispatch");
    }
    if (!writeFile("/INPUT.LL", textInput()) || !writeFile("/INPUT.BC", bitcodeInput().getBuffer()))
        return fail("file fixtures write");
    for (StringRef path : {StringRef("/INPUT.LL"), StringRef("/INPUT.BC")}) {
        LLVMContext fileContext;
        auto module = parseIRFile(path, diagnostic, fileContext);
        if (!module || !matches(*module)) return fail("IRReader file format dispatch");
    }
    LLVMContext missingContext;
    if (parseIRFile("/NOFILE.BC", diagnostic, missingContext) || diagnostic.getMessage().empty())
        return fail("missing IR file accepted");
    record("SH9-d7: IRReader dispatches text and bitcode in memory and on RAM FAT, missing file diagnosed -> OK");

    if (setenv("KUMO_GRANT", "fs", 1) != 0) return fail("grant opt-in environment");
    const StringRef disassemble[] = {"/DIS.ELF", "--show-annotations", "/INPUT.BC", "-o", "/OUTPUT.LL"};
    if (!runDis(disassemble, std::nullopt, 0) || !readContains("/OUTPUT.LL", "[#uses="))
        return fail("native llvm-dis execution");
    LLVMContext toolContext;
    auto result = parseIRFile("/OUTPUT.LL", diagnostic, toolContext);
    auto stderrFile = MemoryBuffer::getFile("/DIS.ERR");
    if (!result || !matches(*result) || !stderrFile || !(*stderrFile)->getBuffer().empty())
        return fail("native llvm-dis output semantics or stderr");
    record("SH9-d7: upstream native llvm-dis reads bitcode and writes reparsable annotated IR in a granted child -> OK");

    const StringRef version[] = {"/DIS.ELF", "--version"};
    if (!runDis(version, StringRef("/VERS.TXT"), 0) || !readContains("/VERS.TXT", "22.1.8")) return fail("native llvm-dis version");
    const StringRef absent[] = {"/DIS.ELF", "/NOFILE.BC", "-o", "/MISSING.LL"};
    if (!runDis(absent, std::nullopt, 1) || !readContains("/DIS.ERR", "NOFILE.BC") ||
        sys::fs::exists("/MISSING.LL")) return fail("native llvm-dis failure path");
    for (StringRef path : {StringRef("/INPUT.LL"), StringRef("/INPUT.BC"), StringRef("/OUTPUT.LL"),
                           StringRef("/VERS.TXT"), StringRef("/DIS.ERR")})
        if (sys::fs::remove(path) || sys::fs::exists(path)) return fail("temporary file cleanup");
    record("SH9-d7: native llvm-dis reports LLVM 22 version and missing-input failure, temporary files removed -> OK");
    record("SH9-d7 LLVM IR readers green");
    return 0;
}
