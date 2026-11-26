# Exact-asset tests on hosted Windows require verified outbound denial.
# Scope both rules to owned paths and remove them on every exit.
param(
    [Parameter(Mandatory=$true)][string]$Artifact,
    [Parameter(Mandatory=$true)][string]$Sha256,
    [Parameter(Mandatory=$true)][string]$Build,
    [Parameter(Mandatory=$true)][string]$Suites,
    [string]$Evidence = "evidence"
)
$ErrorActionPreference = "Stop"
$nonce = [Guid]::NewGuid().ToString("N")
$probeDir = Join-Path $env:RUNNER_TEMP "awman-firewall-$nonce"
$artifactRule = "awman-artifact-$nonce"
$probeRule = "awman-probe-$nonce"
$oldResources = $env:AWMAN_TEST_HOST_RESOURCES
$status = 2
try {
    # Git Bash puts its Unix `link` ahead of MSVC's linker in PATH. The
    # release test compile must use the installed Visual Studio linker.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path $vswhere)) { throw "BLOCKED[msvc-linker]: vswhere is unavailable" }
    $visualStudio = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $visualStudio) { throw "BLOCKED[msvc-linker]: Visual Studio C++ tools are unavailable" }
    $visualStudio = $visualStudio.Trim()
    $toolsets = Join-Path $visualStudio "VC\Tools\MSVC"
    $toolset = Get-ChildItem $toolsets -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if (-not $toolset) { throw "BLOCKED[msvc-linker]: MSVC toolset is unavailable" }
    $compilerDir = Join-Path $toolset.FullName "bin\Hostx64\x64"
    $compiler = Join-Path $compilerDir "cl.exe"
    if (-not (Test-Path $compiler)) { throw "BLOCKED[msvc-compiler]: $compiler is unavailable" }
    $linker = Join-Path $compilerDir "link.exe"
    if (-not (Test-Path $linker)) { throw "BLOCKED[msvc-linker]: $linker is unavailable" }
    # The hosted runner's Git Bash PATH has its own tools first. Make MSVC's
    # compiler and tools available to C build scripts in isolated test runs.
    $env:PATH = "$compilerDir;$env:PATH"
    $env:CC = $compiler
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $linker
    $kit = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10"
    $sdk = Get-ChildItem (Join-Path $kit "Lib") -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if (-not $sdk) { throw "BLOCKED[windows-sdk]: Windows SDK libraries are unavailable" }
    $libraries = @(
        (Join-Path $toolset.FullName "lib\x64"),
        (Join-Path $sdk.FullName "ucrt\x64"),
        (Join-Path $sdk.FullName "um\x64")
    )
    $includes = @(
        (Join-Path $toolset.FullName "include"),
        (Join-Path $kit "Include\$($sdk.Name)\ucrt"),
        (Join-Path $kit "Include\$($sdk.Name)\shared"),
        (Join-Path $kit "Include\$($sdk.Name)\um")
    )
    foreach ($path in @($libraries + $includes)) {
        if (-not (Test-Path $path)) { throw "BLOCKED[windows-sdk]: $path is unavailable" }
    }
    foreach ($library in @(
        (Join-Path $libraries[0] "vcruntime.lib"),
        (Join-Path $libraries[1] "ucrt.lib"),
        (Join-Path $libraries[2] "kernel32.lib")
    )) {
        if (-not (Test-Path $library)) { throw "BLOCKED[windows-sdk]: $library is unavailable" }
    }
    $env:LIB = $libraries -join ';'
    $env:INCLUDE = $includes -join ';'
    $exe = (Resolve-Path $Artifact).Path
    if ((Get-FileHash -Algorithm SHA256 $exe).Hash.ToLowerInvariant() -ne $Sha256) {
        throw "BLOCKED[artifact-digest]: executable digest changed"
    }
    if ((Get-Service MpsSvc).Status -ne "Running") {
        throw "BLOCKED[offline]: Windows firewall service unavailable"
    }
    if (@(Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count -ne 0) {
        throw "BLOCKED[offline]: firewall must enforce every profile"
    }
    New-Item -ItemType Directory -Path $probeDir | Out-Null
    $probe = Join-Path $probeDir "firewall-probe.exe"
    Copy-Item "$env:SystemRoot\System32\curl.exe" $probe
    # A live baseline distinguishes actual denial from a broken probe/network.
    & $probe --fail --silent --show-error --max-time 10 --head https://github.com/ | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "BLOCKED[offline]: firewall probe baseline unavailable" }
    New-NetFirewallRule -Name $probeRule -DisplayName $probeRule -Program $probe -Direction Outbound -Action Block -Profile Any | Out-Null
    & $probe --fail --silent --show-error --max-time 5 --head https://github.com/ | Out-Null
    if ($LASTEXITCODE -eq 0) { throw "BLOCKED[offline]: outbound denial probe failed" }
    New-NetFirewallRule -Name $artifactRule -DisplayName $artifactRule -Program $exe -Direction Outbound -Action Block -Profile Any | Out-Null
    $rule = Get-NetFirewallRule -Name $artifactRule -PolicyStore ActiveStore
    $filter = $rule | Get-NetFirewallApplicationFilter
    if ($rule.Enabled -ne "True" -or $rule.Action -ne "Block" -or $filter.Program -ne $exe) {
        throw "BLOCKED[offline]: exact executable rule not active"
    }
    $env:AWMAN_TEST_HOST_RESOURCES = "offline=windows-firewall"
    $status = 0
    foreach ($suite in $Suites.Split(',')) {
        cargo xtask test run --suite $suite --build $Build --artifact $exe --artifact-sha256 $Sha256 --evidence-dir (Join-Path $Evidence $suite)
        if ($LASTEXITCODE -ne 0) { $status = $LASTEXITCODE }
    }
} finally {
    Remove-NetFirewallRule -Name $artifactRule -ErrorAction SilentlyContinue
    Remove-NetFirewallRule -Name $probeRule -ErrorAction SilentlyContinue
    if (Test-Path $probeDir) { Remove-Item -Recurse -Force $probeDir }
    $env:AWMAN_TEST_HOST_RESOURCES = $oldResources
}
exit $status
