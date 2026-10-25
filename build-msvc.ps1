#requires -Version 5.1
<#
.SYNOPSIS
Build and install this fork's x64 MSVC C API in Release mode.
.DESCRIPTION
Run from any directory using Windows PowerShell 5.1 or PowerShell 7. Requires
Visual Studio C++ build tools and a Windows SDK, rustup, CMake, and Ninja.
Discovers Visual Studio, selects the MSVC Rust toolchain, reconfigures CMake,
builds incrementally, and installs headers and libraries to artifacts/msvc.
Runs the build in a child PowerShell process, leaving the caller's working
directory and environment unchanged. Press Enter at the end to close the script.
.PARAMETER BuildCli
Also build target/msvc-cli/x86_64-pc-windows-msvc/release/wasmtime.exe.
The CLI uses a separate Cargo output directory to avoid overwriting the C API's
wasmtime.lib with the executable's import library.
.PARAMETER UpdateRust
Update the stable MSVC Rust toolchain before building. Without this option,
an installed toolchain is reused and checked against Cargo.toml's rust-version.
A missing toolchain is installed automatically unless Offline is specified.
.PARAMETER Offline
Pass --offline to Cargo and skip any Rust toolchain installation. Dependencies
and the stable MSVC toolchain must already be available locally.
.EXAMPLE
.\build-msvc.ps1
.EXAMPLE
.\build-msvc.ps1 -UpdateRust -BuildCli
.EXAMPLE
.\build-msvc.ps1 -Offline
#>
[CmdletBinding()]
param(
    [switch]$BuildCli,
    [switch]$UpdateRust,
    [switch]$Offline,
    [Parameter(DontShow = $true)][switch]$BuildProcess
)

$ErrorActionPreference = 'Stop'

function Invoke-BuildCommand {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [string[]]$Arguments = @()
    )

    # Stop the build if a native tool reports failure
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "'$Command' failed with exit code $LASTEXITCODE"
    }
}

if ($env:OS -ne 'Windows_NT') {
    throw 'This script requires Windows'
}
if (!$BuildProcess) {
    # Isolate Visual Studio and Rust environment changes from the caller's shell
    $powerShellExecutable = if ($PSVersionTable.PSEdition -eq 'Desktop') { 'powershell.exe' } else { 'pwsh.exe' }
    $buildProcessArguments = @('-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-BuildProcess')
    if ($BuildCli) { $buildProcessArguments += '-BuildCli' }
    if ($UpdateRust) { $buildProcessArguments += '-UpdateRust' }
    if ($Offline) { $buildProcessArguments += '-Offline' }
    try {
        Invoke-BuildCommand (Join-Path $PSHOME $powerShellExecutable) -Arguments $buildProcessArguments
    } catch {
        Write-Host $_ -ForegroundColor Red
        throw
    } finally {
        $null = Read-Host 'Press Enter to close'
    }
    return
}

if ($Offline -and $UpdateRust) {
    throw '-Offline and -UpdateRust cannot be used together'
}

$repoRoot = $PSScriptRoot
$rustToolchain = 'stable-x86_64-pc-windows-msvc'
$rustTarget = 'x86_64-pc-windows-msvc'
$buildDirectory = Join-Path $repoRoot 'target/c-api-msvc'
$installDirectory = Join-Path $repoRoot 'artifacts/msvc'

Push-Location -LiteralPath $repoRoot
try {
    # Make local support tools available before initializing Visual Studio
    $env:Path = 'C:\msys\ucrt64\bin;C:\msys\usr\bin;' + $env:Path
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (!(Test-Path -LiteralPath $vswhere)) {
        throw 'Visual Studio Installer was not found; install the Visual Studio C++ build tools and a Windows SDK'
    }
    $vsInstallation = @(& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
    if ($LASTEXITCODE -ne 0 -or $vsInstallation.Count -eq 0) {
        throw 'No Visual Studio installation with x64 C++ build tools was found'
    }
    $developerShell = Join-Path $vsInstallation[0].Trim() 'Common7/Tools/Launch-VsDevShell.ps1'
    & $developerShell -Arch amd64 -HostArch amd64 -SkipAutomaticLocation
    $cl = (Get-Command cl.exe -CommandType Application | Select-Object -First 1).Source
    $msvcLinker = Join-Path (Split-Path -Parent $cl) 'link.exe'

    # Find rustup independently of the MSYS2 Rust installation on PATH
    $cargoBin = if ($env:CARGO_HOME) {
        Join-Path $env:CARGO_HOME 'bin'
    } else {
        Join-Path $env:USERPROFILE '.cargo/bin'
    }
    $rustup = Join-Path $cargoBin 'rustup.exe'
    if (!(Test-Path -LiteralPath $rustup)) {
        $rustupCommand = Get-Command rustup.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -eq $rustupCommand) {
            throw 'rustup was not found; install Rust with rustup before running this script'
        }
        $rustup = $rustupCommand.Source
    }
    $installedToolchains = @(& $rustup toolchain list)
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not list the installed Rust toolchains'
    }
    if ($UpdateRust -or !($installedToolchains -match '^stable-x86_64-pc-windows-msvc(\s|$)')) {
        if ($Offline) {
            throw 'The MSVC Rust toolchain is missing; run this script once without -Offline'
        }
        Invoke-BuildCommand $rustup -Arguments @('toolchain', 'install', $rustToolchain, '--profile', 'minimal')
    }

    # Use the MSVC toolchain's actual executables rather than whichever Cargo is on PATH
    $cargo = & $rustup which --toolchain $rustToolchain cargo
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not locate Cargo in the MSVC Rust toolchain'
    }
    $rustc = & $rustup which --toolchain $rustToolchain rustc
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not locate rustc in the MSVC Rust toolchain'
    }
    $env:Path = (Split-Path -Parent $cargo) + ';' + $env:Path
    $env:RUSTUP_TOOLCHAIN = $rustToolchain
    $env:RUSTC = $rustc
    $env:CARGO_TARGET_DIR = (Join-Path $repoRoot 'target').Replace('\', '/')
    $env:CC_x86_64_pc_windows_msvc = $cl
    $env:CXX_x86_64_pc_windows_msvc = $cl
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $msvcLinker
    $cmake = (Get-Command cmake.exe -CommandType Application | Select-Object -First 1).Source
    $null = Get-Command ninja.exe -CommandType Application

    # Check this checkout's minimum Rust version before starting a potentially long build
    $manifest = [IO.File]::ReadAllText((Join-Path $repoRoot 'Cargo.toml'))
    $minimumRust = [regex]::Match($manifest, '(?m)^rust-version\s*=\s*"([0-9.]+)"')
    $rustVersionOutput = & $rustc --version
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not determine the installed Rust version'
    }
    $installedRust = [regex]::Match($rustVersionOutput, '^rustc ([0-9.]+)')
    if (!$minimumRust.Success -or !$installedRust.Success) {
        throw 'Could not parse the required or installed Rust version'
    }
    if ([version]$installedRust.Groups[1].Value -lt [version]$minimumRust.Groups[1].Value) {
        throw "This checkout requires Rust $($minimumRust.Groups[1].Value) or newer; rerun with -UpdateRust"
    }
    Write-Host "Building with $rustVersionOutput"

    # Reconfigure after repository updates while retaining incremental Cargo artifacts
    $cargoOptions = if ($Offline) { '--locked;--offline' } else { '--locked' }
    $configureArguments = @(
        '-S', (Join-Path $repoRoot 'crates/c-api'),
        '-B', $buildDirectory,
        '-G', 'Ninja',
        "-DCMAKE_C_COMPILER:FILEPATH=$cl",
        "-DCMAKE_LINKER:FILEPATH=$msvcLinker",
        '-DCMAKE_BUILD_TYPE=Release',
        "-DWASMTIME_TARGET=$rustTarget",
        "-DWASMTIME_CARGO_BINARY:FILEPATH=$cargo",
        "-DWASMTIME_USER_CARGO_BUILD_OPTIONS:STRING=$cargoOptions",
        '--install-prefix', $installDirectory
    )
    Invoke-BuildCommand $cmake -Arguments $configureArguments

    # Reapply options if a compiler change made CMake reset its cache during the first pass
    Invoke-BuildCommand $cmake -Arguments $configureArguments
    $configuredCache = [IO.File]::ReadAllText((Join-Path $buildDirectory 'CMakeCache.txt'))
    $expectedCache = @{
        CMAKE_BUILD_TYPE = 'Release'
        CMAKE_INSTALL_PREFIX = $installDirectory
        WASMTIME_TARGET = $rustTarget
        WASMTIME_CARGO_BINARY = $cargo
        WASMTIME_USER_CARGO_BUILD_OPTIONS = $cargoOptions
    }
    foreach ($cacheVariable in $expectedCache.Keys) {
        $cacheValue = [regex]::Match($configuredCache, '(?m)^' + $cacheVariable + ':[^=\r\n]+=([^\r\n]*)')
        if (!$cacheValue.Success -or
            $cacheValue.Groups[1].Value.Replace('\', '/') -ne $expectedCache[$cacheVariable].Replace('\', '/')) {
            throw "CMake did not retain the requested $cacheVariable setting; refusing to build or install"
        }
    }

    # Compile and install only after confirming the Release profile and intended destination
    Invoke-BuildCommand $cmake -Arguments @('--build', $buildDirectory)
    Invoke-BuildCommand $cmake -Arguments @('--install', $buildDirectory)

    # Isolate the CLI because its MSVC import library has the same name as the C API static library
    if ($BuildCli) {
        $cliBuildDirectory = Join-Path $repoRoot 'target/msvc-cli'
        $env:CARGO_TARGET_DIR = $cliBuildDirectory.Replace('\', '/')
        $cliArguments = @('build', '--locked', '--release', '--target', $rustTarget,
            '--target-dir', $cliBuildDirectory, '-p', 'wasmtime-cli')
        if ($Offline) {
            $cliArguments += '--offline'
        }
        Write-Host "Building CLI in isolated directory: $cliBuildDirectory"
        Invoke-BuildCommand $cargo -Arguments $cliArguments
        Write-Host "CLI: $(Join-Path $cliBuildDirectory 'x86_64-pc-windows-msvc/release/wasmtime.exe')"
    }
    Write-Host "C API headers and libraries installed in: $installDirectory"
} finally {
    # Restore the build process's directory before exiting
    Pop-Location
}
