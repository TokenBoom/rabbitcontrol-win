# RabbitControl Windows 发布打包（M6 v1）：
# cargo build --locked --release → 产物 + 文档打包 zip。
# MSIX/签名待后续；CI 发布走 .github/workflows/release.yml。
#
# 用法：powershell -ExecutionPolicy Bypass -File scripts\package-win.ps1 [-OutDir dist]

param(
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot

Push-Location $repoRoot
try {
    Write-Host "==> cargo build --locked --release"
    cargo build --locked --release -p rc-ui -p rc-daemon
    if ($LASTEXITCODE -ne 0) { throw "release 构建失败" }

    $exe = Join-Path $repoRoot "target\release\rabbitcontrol-win.exe"
    $daemon = Join-Path $repoRoot "target\release\rc-daemon.exe"
    if (-not (Test-Path $exe)) { throw "未找到产物 $exe" }
    if (-not (Test-Path $daemon)) { throw "未找到产物 $daemon（开机自启服务）" }

    $version = (& $exe --version 2>$null) -replace '^\S+\s+', ''
    if ([string]::IsNullOrWhiteSpace($version)) { $version = "0.1.0" }
    $stage = Join-Path $repoRoot "$OutDir\rabbitcontrol-win-$version"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    Copy-Item $exe $stage
    Copy-Item $daemon $stage
    foreach ($doc in @("README.md", "LICENSE")) {
        $src = Join-Path $repoRoot $doc
        if (Test-Path $src) { Copy-Item $src $stage }
    }

    $zip = "$stage.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Write-Host "==> Compress-Archive → $zip"
    Compress-Archive -Path $stage -DestinationPath $zip
    Write-Host "打包完成：$zip"
} finally {
    Pop-Location
}
