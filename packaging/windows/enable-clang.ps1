# ring's Windows ARM64 crypto backend requires clang. Release builds remain
# self-contained; this is a build-time tool, not a runtime dependency.
param([Parameter(Mandatory = $true)][string]$Target)

if ($Target -ne "aarch64-pc-windows-msvc") { return }
if (Get-Command clang -ErrorAction SilentlyContinue) { return }

$candidates = @("$env:ProgramFiles\LLVM\bin")
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
    $installations = & $vswhere -all -products '*' -property installationPath
    foreach ($installation in $installations) {
        $candidates += Join-Path $installation "VC\Tools\Llvm\ARM64\bin"
        $candidates += Join-Path $installation "VC\Tools\Llvm\bin"
    }
}
foreach ($candidate in $candidates) {
    if (Test-Path (Join-Path $candidate "clang.exe")) {
        $env:PATH = "$candidate;$env:PATH"
        return
    }
}
throw "Internet netplay requires LLVM clang for Windows ARM64. Install LLVM or Visual Studio's C++ Clang tools and add clang.exe to PATH."
