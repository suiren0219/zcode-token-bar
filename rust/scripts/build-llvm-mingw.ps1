param(
    [Parameter(Mandatory = $true)][string]$ToolchainRoot,
    [ValidateSet('build','test','check')][string]$Action = 'build'
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$toolRoot = (Resolve-Path $ToolchainRoot).Path
$compilerBin = Join-Path $toolRoot 'bin'
if (-not (Test-Path (Join-Path $compilerBin 'x86_64-w64-mingw32-gcc.exe'))) {
    throw 'ToolchainRoot must point to the LLVM-MinGW root directory containing bin/x86_64-w64-mingw32-gcc.exe.'
}
$builtins = Get-ChildItem (Join-Path $toolRoot 'lib/clang') -Recurse -Filter 'libclang_rt.builtins-x86_64.a' |
    Where-Object { $_.Directory.Name -eq 'windows' } | Select-Object -First 1
$unwind = Join-Path $toolRoot 'x86_64-w64-mingw32/lib/libunwind.a'
if (-not $builtins -or -not (Test-Path $unwind)) { throw 'LLVM-MinGW is missing the x86_64 compiler-rt builtins library or libunwind.a.' }
$compatRoot = Join-Path $projectRoot 'target/llvm-mingw-compat'
New-Item -ItemType Directory -Force -Path $compatRoot | Out-Null
Copy-Item -LiteralPath $builtins.FullName -Destination (Join-Path $compatRoot 'libgcc.a') -Force
Copy-Item -LiteralPath $unwind -Destination (Join-Path $compatRoot 'libgcc_eh.a') -Force
$previousPath = $env:PATH
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$previousRustFlags = $env:RUSTFLAGS
try {
    $env:PATH = "$compilerBin;$previousPath"
    $env:RUSTFLAGS = $null
    $env:CARGO_ENCODED_RUSTFLAGS = '-L' + [char]31 + ('native=' + $compatRoot)
    Push-Location $projectRoot
    try {
        if ($Action -eq 'test') { & cargo test --release -- --test-threads=1 }
        elseif ($Action -eq 'check') { & cargo check }
        else { & cargo build --release }
        $buildExit = $LASTEXITCODE
    } finally { Pop-Location }
} finally {
    $env:PATH = $previousPath
    $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags
    $env:RUSTFLAGS = $previousRustFlags
}
exit $buildExit
