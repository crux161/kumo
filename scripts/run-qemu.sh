#!/bin/sh

cp -v /opt/homebrew/share/qemu/edk2-arm-vars.fd ./kumo-vars.fd

qemu-system-aarch64 -M virt,gic-version=3 \
	-cpu cortex-a72 -m 512 -display none \
	-serial stdio -monitor none   \
	-drive if=pflash,format=raw,file=/opt/homebrew/share/qemu/edk2-aarch64-code.fd,readonly=on \
	-drive if=pflash,format=raw,file=./kumo-vars.fd  \
	-drive file=fat:rw:/Users/crux/git/soso/KUMO/build/images/qemu-virt-aarch64,format=raw,if=virtio
