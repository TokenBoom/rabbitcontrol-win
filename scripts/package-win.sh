#!/usr/bin/env bash
# 打包 Windows 安装产物（骨架占位，对应 scripts/package-macos.sh）
# TODO: release 构建 + MSIX/MSI 打包 + 签名
set -euo pipefail
cargo build --locked --release
