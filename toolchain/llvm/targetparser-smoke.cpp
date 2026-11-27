//J950_r16_001
// SH9-d3: native target/triple helpers and BinaryFormat's complete TargetParser link
// dependency. CPU feature queries conservatively use upstream's generic fallback. — KESTREL
#include "llvm/ADT/SmallVector.h"
#include "llvm/BinaryFormat/MachO.h"
#include "llvm/BinaryFormat/XCOFF.h"
#include "llvm/Support/Error.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/TargetParser/AArch64TargetParser.h"
#include "llvm/TargetParser/ARMTargetParser.h"
#include "llvm/TargetParser/CSKYTargetParser.h"
#include "llvm/TargetParser/Host.h"
#include "llvm/TargetParser/LoongArchTargetParser.h"
#include "llvm/TargetParser/PPCTargetParser.h"
#include "llvm/TargetParser/RISCVISAInfo.h"
#include "llvm/TargetParser/RISCVTargetParser.h"
#include "llvm/TargetParser/SubtargetFeature.h"
#include "llvm/TargetParser/TargetParser.h"
#include "llvm/TargetParser/Triple.h"
#include "llvm/TargetParser/X86TargetParser.h"
#include "llvm/TargetParser/XtensaTargetParser.h"

using namespace llvm;

static int fail(const char* why) {
    outs() << "SH9-d3: " << why << "\nSH9-d3 LLVM TargetParser FAILED\n";
    outs().flush();
    return 1;
}

static void record(StringRef text) {
    outs() << text << '\n';
    outs().flush();
}

extern "C" int main() {
    record("SH9-d3 LLVM TargetParser begin");
    Triple native("aarch64-unknown-kumo");
    if (native.getOS() != Triple::Kumo || Triple::getOSTypeName(native.getOS()) != "kumo" ||
        native.getArch() != Triple::aarch64 || !native.isArch64Bit() || native.isArch32Bit() ||
        !native.isLittleEndian() || !native.isOSBinFormatELF() ||
        Triple::normalize("aarch64-kumo") != "aarch64-unknown-kumo" ||
        Triple("aarch64-unknown-kumox").getOS() != Triple::UnknownOS ||
        native.computeDataLayout() != "e-m:e-p270:32:32-p271:32:32-p272:64:64-i8:8:32-i16:16:32-"
                                      "i64:64-i128:128-n32:64-S128-Fn32")
        return fail("KUMO triple, normalization or data layout");
    if (sys::getDefaultTargetTriple() != "aarch64-unknown-kumo" ||
        sys::getProcessTriple() != "aarch64-unknown-kumo" ||
        sys::getHostCPUName() != "generic" || !sys::getHostCPUFeatures().empty())
        return fail("host triple or conservative CPU detection");
    record("SH9-d3: KUMO triple, normalization, data layout and conservative host queries -> OK");

    {
        auto arch = AArch64::parseArch("armv8.2-a");
        auto cpu = AArch64::parseCpu("cortex-a76");
        auto crc = AArch64::parseArchExtension("crc");
        if (!arch || arch->Version != VersionTuple(8, 2) || !cpu || cpu->Arch != *arch ||
            !crc || crc->PosTargetFeature != "+crc" ||
            AArch64::parseArch("invalid") || AArch64::parseCpu("invalid") ||
            AArch64::parseArchExtension("invalid") ||
            ARM::parseArch("armv7-a") != ARM::ArchKind::ARMV7A ||
            ARM::parseArch("invalid") != ARM::ArchKind::INVALID)
            return fail("AArch64 generated architecture/CPU/extension tables or ARM parser");
        std::vector<StringRef> features;
        if (!AArch64::getExtensionFeatures(cpu->getImpliedExtensions(), features) || features.empty())
            return fail("AArch64 CPU extensions");
    }
    record("SH9-d3: generated AArch64 architectures, CPUs and extensions, ARM parsing -> OK");

    {
        auto isa = RISCVISAInfo::parseArchString("rv64imafdc_zicsr_zifencei", false);
        if (!isa) { consumeError(isa.takeError()); return fail("RISC-V ISA rejected"); }
        if ((*isa)->getXLen() != 64 || (*isa)->getFLen() != 64 || !(*isa)->hasExtension("c") ||
            (*isa)->computeDefaultABI() != "lp64d" || (*isa)->toFeatures().empty() ||
            !RISCV::parseCPU("generic-rv64", true) || RISCV::parseCPU("generic-rv32", true) ||
            RISCV::parseCPU("invalid", true))
            return fail("RISC-V ISA, ABI or generated CPU table");
        auto invalid = RISCVISAInfo::parseArchString("rv65i", false);
        if (invalid) return fail("invalid RISC-V ISA accepted");
        if (toString(invalid.takeError()).empty()) return fail("RISC-V error text");
    }
    record("SH9-d3: generated RISC-V CPUs, ISA extensions, ABI and invalid input -> OK");

    {
        if (PPC::normalizeCPUName("power8") != "pwr8" || !PPC::isValidCPU("pwr8") ||
            PPC::isValidCPU("invalid")) return fail("PPC CPU names");
        auto ppc = PPC::getPPCDefaultTargetFeatures(Triple("powerpc64le-unknown-linux-gnu"), "pwr8");
        if (!ppc || !ppc->lookup("altivec") ||
            X86::parseArchX86("haswell", true) != X86::CK_Haswell ||
            X86::parseArchX86("invalid", true) != X86::CK_None ||
            !LoongArch::isValidCPUName("la464") || LoongArch::isValidCPUName("invalid") ||
            CSKY::parseArch("ck810") == CSKY::ArchKind::INVALID ||
            CSKY::parseArch("invalid") != CSKY::ArchKind::INVALID ||
            Xtensa::parseCPUKind("esp32") == Xtensa::CK_INVALID ||
            Xtensa::parseCPUKind("invalid") != Xtensa::CK_INVALID)
            return fail("PPC generated features or X86/LoongArch/CSKY/Xtensa tables");
    }
    record("SH9-d3: generated PPC features and X86, LoongArch, CSKY and Xtensa tables -> OK");

    {
        SubtargetFeatures features("+neon,-crc");
        features.AddFeature("aes");
        if (features.getString() != "+neon,-crc,+aes" || features.getFeatures().size() != 3)
            return fail("subtarget feature list");
        FeatureBitset bits{1, 65, 190};
        bits.reset(65);
        bits.flip(2);
        if (!bits.test(1) || bits.test(65) || !bits.test(190) || bits.count() != 3 ||
            AMDGPU::parseArchAMDGCN("gfx900") != AMDGPU::GK_GFX900 ||
            AMDGPU::parseArchAMDGCN("invalid") != AMDGPU::GK_NONE)
            return fail("feature bitsets or AMDGPU architecture table");
    }
    record("SH9-d3: subtarget feature lists, wide feature bitsets and AMDGPU names -> OK");

    {
        auto arm64 = MachO::getCPUType(Triple("arm64-apple-macosx"));
        auto armv7 = MachO::getCPUSubType(Triple("armv7-apple-ios"));
        auto x86 = MachO::getCPUSubType(Triple("x86_64h-apple-macosx"));
        if (!arm64 || !armv7 || !x86) {
            if (!arm64) consumeError(arm64.takeError());
            if (!armv7) consumeError(armv7.takeError());
            if (!x86) consumeError(x86.takeError());
            return fail("Mach-O CPU helper error");
        }
        if (*arm64 != MachO::CPU_TYPE_ARM64 || *armv7 != MachO::CPU_SUBTYPE_ARM_V7 ||
            *x86 != MachO::CPU_SUBTYPE_X86_64_H || XCOFF::getCpuID("power8") != XCOFF::TCPU_PWR8)
            return fail("Mach-O or XCOFF TargetParser dependency");
        auto unsupported = MachO::getCPUType(native);
        if (unsupported) return fail("non-Mach-O triple accepted");
        if (!StringRef(toString(unsupported.takeError())).contains("Unsupported triple"))
            return fail("Mach-O unsupported-triple error");
    }
    record("SH9-d3: BinaryFormat Mach-O and XCOFF dependencies, unsupported-triple error -> OK");
    record("SH9-d3 LLVM TargetParser green");
    return 0;
}
