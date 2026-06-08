# ThinkPad X13s Gen 1 Hardware Target

Status: first named arm64 metal target.

Identity:
- Product: Lenovo ThinkPad X13s Gen 1
- SoC: Qualcomm Snapdragon 8cx Gen 3 / SC8280XP
- Arch: aarch64
- Interrupt controller: GICv3 path
- Firmware path: UEFI

Boot contract:
- ESP fallback loader: `EFI/BOOT/BOOTAA64.EFI`
- Source DTB in this workspace: `sc8280xp-lenovo-thinkpad-x13s.dtb`
- DTB expected on ESP: `EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb`
- DTB compatibles: `lenovo,thinkpad-x13s`, `qcom,sc8280xp`
- Firmware setup for unsigned early KUMO: update firmware, enable Linux Boot, disable Secure Boot.

Image staging:
- `cargo xtask image --arch aarch64 --hardware x13s` builds `niji-uefi` for `aarch64-unknown-uefi`, validates the PE/COFF output, and stages it as `EFI/BOOT/BOOTAA64.EFI`.
- The same command validates the source DTB root model/compatibles before staging it.
- Staged bootloader path: `build/images/thinkpad-x13s-gen1/EFI/BOOT/BOOTAA64.EFI`
- Staged DTB path: `build/images/thinkpad-x13s-gen1/EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb`
- The generated image manifest records bootloader and DTB source, ESP destination, staged path, byte size, fingerprint, and model where available.

Early debug:
- Do not assume an exposed UART.
- First hardware-visible output should be UEFI console/GOP-backed when Nijigumo becomes a real UEFI app.
- The raw QEMU PL011 path remains an emulator smoke only.

Non-goals for first light:
- Accelerated Adreno graphics.
- WWAN, audio, suspend/resume, camera, fingerprint reader.
- Secure Boot signing.
