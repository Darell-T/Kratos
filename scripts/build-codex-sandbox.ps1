param(
    [Parameter(Mandatory = $true)]
    [string]$CodexExe
)

$ErrorActionPreference = 'Stop'
$CodexExe = (Resolve-Path -LiteralPath $CodexExe).Path
$version = & $CodexExe --version
if ($LASTEXITCODE -ne 0 -or $version -ne 'codex-cli 0.159.1') {
    throw 'This patch requires a separate Codex 0.159.1 installation'
}
$resources = Join-Path (Split-Path -Parent (Split-Path -Parent $CodexExe)) 'codex-resources'
$helper = Join-Path $resources 'codex-windows-sandbox-setup.exe'
if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
    throw 'The Codex installation has no native Windows sandbox helper'
}
$patch = Join-Path $PSScriptRoot 'codex-sandbox-read-grants.patch'
$build = Join-Path ([IO.Path]::GetTempPath()) ('kratos-codex-build-' + [guid]::NewGuid())
$build = [IO.Path]::GetFullPath($build)
$source = Join-Path $build 'codex'
$replacement = $null
New-Item -ItemType Directory -Path $build | Out-Null
try {
    git clone --config core.autocrlf=false --sparse --depth 1 --single-branch `
        --branch rust-v0.159.1 `
        https://github.com/openai/codex.git $source
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not download the pinned Codex source'
    }
    $revision = git -C $source rev-parse HEAD
    if ($LASTEXITCODE -ne 0 -or $revision -ne '8e68a98ef03cdde76d2e6800791ebdf1b3b95b24') {
        throw 'The Codex source differs from the pinned revision'
    }
    # Unused snapshots exceed Windows path limits in a temporary source checkout.
    git -C $source sparse-checkout set --no-cone '/*' '!**/snapshots/' '!**/*.snap'
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not check out the Codex build source'
    }
    git -C $source apply --check $patch
    if ($LASTEXITCODE -ne 0) {
        throw 'The sandbox patch does not match the pinned source'
    }
    git -C $source apply $patch
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not apply the sandbox patch'
    }
    rustup toolchain install 1.95.0 --profile minimal --no-self-update
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not install the pinned Codex build toolchain'
    }

    Push-Location (Join-Path $source 'codex-rs')
    try {
        # The release tag changes workspace versions without updating Cargo.lock.
        cargo +1.95.0 update --workspace
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not align the release workspace versions'
        }
        $changes = git diff --unified=0 -- Cargo.lock
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not check the Codex dependency lock'
        }
        $unexpected = $changes | Where-Object {
            $_ -match '^[+-](?![+-])' -and
            $_ -ne '-version = "0.0.0"' -and $_ -ne '+version = "0.159.1"'
        }
        if ($unexpected) {
            throw 'The Codex build changed an external dependency'
        }
        cargo +1.95.0 build --locked --release -p codex-windows-sandbox `
            --bin codex-windows-sandbox-setup --target-dir (Join-Path $build 'target')
        if ($LASTEXITCODE -ne 0) {
            throw 'Could not build the patched sandbox helper'
        }
    } finally {
        Pop-Location
    }

    $replacement = Join-Path $resources ('kratos-sandbox-' + [guid]::NewGuid() + '.tmp')
    Copy-Item -LiteralPath (Join-Path $build 'target/release/codex-windows-sandbox-setup.exe') `
        -Destination $replacement
    Copy-Item -LiteralPath (Join-Path $source 'LICENSE') `
        -Destination (Join-Path $resources 'LICENSE.kratos-sandbox')
    $notice = "Kratos changes this helper to finish Windows read grants before commands start.`n"
    $notice += [IO.File]::ReadAllText((Join-Path $source 'NOTICE'))
    [IO.File]::WriteAllText((Join-Path $resources 'NOTICE.kratos-sandbox'), $notice)
    [IO.File]::Replace($replacement, $helper, [NullString]::Value)
    Write-Output 'Installed the patched Codex 0.159.1 Windows sandbox helper'
} finally {
    if ($replacement -and (Test-Path -LiteralPath $replacement)) {
        Remove-Item -LiteralPath $replacement -Force
    }
    $resolved = (Resolve-Path -LiteralPath $build).Path
    if ($resolved -ne $build) {
        throw 'Unexpected Codex build cleanup path'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
