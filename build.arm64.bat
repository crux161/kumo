@echo off
:: Windows Arm64 build script for thinkpad x13s gen 1
cargo xtask image --arch aarch64 --hardware thinkpad-x13s-gen1
