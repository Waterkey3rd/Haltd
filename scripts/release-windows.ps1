# Builds the Windows package into release/. Needs Rust (MSVC) and Node on this
# machine only; the result needs neither.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$env:npm_config_cache = Join-Path $root ".cache\npm"

function Invoke-Step([string]$Directory, [string]$Program, [string[]]$Arguments) {
    Push-Location $Directory
    & $Program @Arguments
    $code = $LASTEXITCODE
    Pop-Location
    if ($code -ne 0) { Write-Host "FAILED: $Program $Arguments"; exit 1 }
}

foreach ($project in "webui", "vscode-ext") {
    if (-not (Test-Path "$project\node_modules")) {
        Invoke-Step $project "npm.cmd" @("ci", "--no-audit", "--no-fund")
    }
}
# The web UI is embedded in the executable, so it has to be built first.
Invoke-Step "webui" "npm.cmd" @("run", "build")
Invoke-Step "vscode-ext" "npm.cmd" @("run", "typecheck")
Invoke-Step "vscode-ext" "npm.cmd" @("run", "build")

# A static C runtime, so the target machine needs no Visual C++ redistributable.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
Invoke-Step "." "cargo" @("build", "--profile", "dist", "-p", "jlinkd", "--locked")
Remove-Item Env:RUSTFLAGS

$version = (Get-Content "vscode-ext\package.json" -Raw -Encoding UTF8 | ConvertFrom-Json).version
$name = "jlinkd-$version-windows-x64"
$output = Join-Path $root "release\$name"
if (Test-Path "release") { Remove-Item "release" -Recurse -Force -Confirm:$false }
New-Item -ItemType Directory -Force $output | Out-Null
New-Item -ItemType Directory -Force "vscode-ext\bin" | Out-Null
Copy-Item "target\dist\jlinkd.exe" "vscode-ext\bin\jlinkd.exe"
Copy-Item "target\dist\jlinkd.exe" "$output\jlinkd.exe"
Copy-Item "docs\windows-usage.md" "$output\README.md"

$vsix = "$output\jlinkd-$version-win32-x64.vsix"
Invoke-Step "vscode-ext" "npx.cmd" @("--yes", "@vscode/vsce@3", "package", "--target", "win32-x64",
    "--no-dependencies", "--allow-missing-repository", "--skip-license", "--out", $vsix)

Compress-Archive -Path $output -DestinationPath "release\$name.zip"
Get-ChildItem "release", $output -File | Select-Object FullName, Length | Format-Table -AutoSize
Get-FileHash "release\$name.zip" -Algorithm SHA256 | Format-List Algorithm, Hash
