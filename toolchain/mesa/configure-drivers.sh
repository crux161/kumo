#!/bin/sh
#j735
set -eu
export PATH=/work/build/src/compiler/clc:/work/build/src/compiler/spirv:/work/build/src/panfrost/clc:$PATH
meson setup /work/kumo-mesa/build /work/kumo-mesa/mesa-26.2.5 --cross-file /work/kumo-mesa/kumo-aarch64.cross --wrap-mode=nodownload -Dbuildtype=release -Dgallium-drivers=panfrost -Dvulkan-drivers=panfrost -Dplatforms= -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dopengl=false -Dgles1=disabled -Dgles2=enabled -Dllvm=disabled -Dxmlconfig=disabled -Dshader-cache=disabled -Dperfetto=false -Dvalgrind=disabled -Dzstd=disabled -Dzlib=disabled -Dexpat=disabled -Ddisplay-info=disabled -Dbuild-tests=false -Dtools= -Dvulkan-layers= -Ddefault_library=static -Dmesa-clc=system -Dprecomp-compiler=system
