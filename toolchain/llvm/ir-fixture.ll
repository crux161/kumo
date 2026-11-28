;J959_r25_001
source_filename = "SH9D7.C"
target datalayout = "e-m:e-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32"
target triple = "aarch64-unknown-kumo"

@answer = constant i32 42, align 4

define i32 @choose(i32 %left, i32 %right) #0 !dbg !4 {
entry:
  %larger = icmp sgt i32 %left, %right
  br i1 %larger, label %yes, label %no
yes:
  br label %merge
no:
  br label %merge
merge:
  %chosen = phi i32 [ %left, %yes ], [ %right, %no ]
  ret i32 %chosen, !dbg !8
}

define i32 @helper() {
entry:
  ret i32 7
}

declare i32 @llvm.bswap.i32(i32)
attributes #0 = { nounwind }

!llvm.dbg.cu = !{!0}
!llvm.module.flags = !{!9}
!kumo.proof = !{!10}
!0 = distinct !DICompileUnit(language: DW_LANG_C, file: !1, producer: "KUMO SH9-d7", isOptimized: false, runtimeVersion: 0, emissionKind: FullDebug)
!1 = !DIFile(filename: "SH9D7.C", directory: "/")
!2 = !DIBasicType(name: "int", size: 32, encoding: DW_ATE_signed)
!3 = !DISubroutineType(types: !5)
!4 = distinct !DISubprogram(name: "choose", scope: !1, file: !1, line: 1, type: !3, scopeLine: 1, flags: DIFlagPrototyped, spFlags: DISPFlagDefinition, unit: !0)
!5 = !{!2, !2, !2}
!8 = !DILocation(line: 17, column: 3, scope: !4)
!9 = !{i32 2, !"Debug Info Version", i32 3}
!10 = !{!"native IR reader"}
