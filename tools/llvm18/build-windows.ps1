# Build the Windows llvm18 bundle by curating the official LLVM 18.1.8
# x86_64-pc-windows-msvc prebuilt. Runs on the Windows x86_64 machine.
# Requires PowerShell 5+, tar (bundled with Windows 10+), and zstd on PATH
# (winget install facebook.zstandard, or scoop install zstd).
#
# Usage:  powershell -ExecutionPolicy Bypass -File tools\llvm18\build-windows.ps1

$ErrorActionPreference = 'Stop'
$Version = '18.1.8'
$Major = '18'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$Out = Join-Path $Here 'out'
$Work = Join-Path $Here 'work'
New-Item -ItemType Directory -Force -Path $Out, $Work | Out-Null

$Prebuilt = "clang+llvm-$Version-x86_64-pc-windows-msvc.tar.xz"
$Url = "https://github.com/llvm/llvm-project/releases/download/llvmorg-$Version/$Prebuilt"
$Tarball = Join-Path $Work $Prebuilt
$Tree = Join-Path $Work 'llvm-win'

if (-not (Test-Path $Tarball)) {
  Write-Host "== download $Prebuilt"
  Invoke-WebRequest -Uri $Url -OutFile $Tarball
}

Write-Host "== extract"
Remove-Item -Recurse -Force $Tree -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Tree | Out-Null
tar -xf $Tarball -C $Tree --strip-components=1

$Bundle = Join-Path $Out 'llvm18-windows-x86_64'
Remove-Item -Recurse -Force $Bundle -ErrorAction SilentlyContinue
$Bin = Join-Path $Bundle 'bin'
$ResInclude = Join-Path $Bundle "lib\clang\$Major\include"
New-Item -ItemType Directory -Force -Path $Bin, $ResInclude | Out-Null

Write-Host "== stage tools"
function Copy-Tool($name) {
  $src = Join-Path $Tree "bin\$name.exe"
  if (-not (Test-Path $src)) { throw "missing tool: $name.exe" }
  Copy-Item $src (Join-Path $Bin "$name.exe")
}
Copy-Tool 'clang'
Copy-Tool 'llvm-rc'
Copy-Tool 'llvm-lipo'

Write-Host "== stage resource headers"
$SrcInclude = Join-Path $Tree "lib\clang\$Major\include"
if (-not (Test-Path $SrcInclude)) { throw "missing resource headers: $SrcInclude" }
Copy-Item -Recurse -Force (Join-Path $SrcInclude '*') $ResInclude

Write-Host "== validate"
$V = Join-Path $Work 'validate'
Remove-Item -Recurse -Force $V -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $V | Out-Null
Set-Content -Path (Join-Path $V 'res.rc') -Value @"
#define APPVER 1,0,0,0
STRINGTABLE BEGIN 1 "peko" END
"@
Push-Location $V
& (Join-Path $Bin 'llvm-rc.exe') 'res.rc' 2> err.txt | Out-Null
Pop-Location
if (-not (Test-Path (Join-Path $V 'res.res'))) {
  Get-Content (Join-Path $V 'err.txt')
  throw "validation failed: no res.res produced"
}
if (Select-String -Path (Join-Path $V 'err.txt') -Pattern 'Unable to find clang' -Quiet) {
  throw "validation failed: llvm-rc could not find sibling clang"
}
Write-Host "validation ok: $Bundle"

$Size = "{0:N0}" -f ((Get-ChildItem -Recurse $Bundle | Measure-Object Length -Sum).Sum)
Write-Host "staged: $Bundle ($Size bytes)"
