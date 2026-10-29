$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$engineExe = Join-Path $repo 'target/release/kratos-engine.exe'
$clientExe = Join-Path $repo 'target/release/kratos-client.exe'
$fakeExe = Join-Path $repo 'target/release/fake-codex.exe'
foreach ($file in @($engineExe, $clientExe, $fakeExe)) {
    if (-not (Test-Path -LiteralPath $file)) { throw 'Run cargo build --release --workspace --locked first' }
}
$probeHome = Join-Path ([IO.Path]::GetTempPath()) ('kratos-turn-bench-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $probeHome | Out-Null
$previous = @{}
foreach ($name in @('KRATOS_HOME', 'KRATOS_PORT', 'CODEX_EXE', 'FAKE_CODEX_CONTROL_FILE')) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$engineProcess = $null
try {
    $env:KRATOS_HOME = $probeHome
    $env:KRATOS_PORT = '0'
    $env:CODEX_EXE = $fakeExe
    $control = Join-Path $probeHome 'fake-codex-control'
    $env:FAKE_CODEX_CONTROL_FILE = $control
    $stdout = Join-Path $probeHome 'stdout.log'
    $stderr = Join-Path $probeHome 'stderr.log'
    $engineProcess = Start-Process -FilePath $engineExe -WorkingDirectory $repo -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $announced = Get-Content -LiteralPath $stdout -Raw -ErrorAction SilentlyContinue
        if ($announced -match 'listening on (http://127.0.0.1:\d+)') { $url = $Matches[1]; break }
        if ($engineProcess.HasExited -or [DateTime]::UtcNow -ge $deadline) { throw (Get-Content -LiteralPath $stderr -Raw) }
        Start-Sleep -Milliseconds 20
    } while ($true)
    $measurement = & $clientExe --url $url bench-turns $control
    if ($LASTEXITCODE -ne 0) { throw 'Turn benchmark failed' }
    $timings = $measurement | ConvertFrom-Json
    $engineProcess.Refresh()
    $workingSet = $engineProcess.WorkingSet64
    $fakeProcess = Get-CimInstance Win32_Process -Filter "ParentProcessId=$($engineProcess.Id)" |
        Where-Object Name -eq 'fake-codex.exe' |
        ForEach-Object { Get-Process -Id $_.ProcessId }
    if (@($fakeProcess).Count -ne 1) { throw 'Expected exactly one fake-codex child' }
    $report = [ordered]@{
        build = 'release'; platform = [Environment]::OSVersion.VersionString
        cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name)
        timing = $timings; engineWorkingSetBytes = $workingSet
        engineAndFakeWorkingSetBytes = $workingSet + $fakeProcess.WorkingSet64
        limits = @{ p95Ms = 5; engineWorkingSetBytes = 60 * 1024 * 1024 }
    }
    $report | ConvertTo-Json -Depth 8 -Compress
    if ($timings.dispatch.p95Ms -gt 5 -or $timings.completion.p95Ms -gt 5 -or $workingSet -gt 60 * 1024 * 1024) {
        throw 'R3 performance budget exceeded'
    }
} finally {
    if ($null -ne $engineProcess -and -not $engineProcess.HasExited) {
        $engineProcess.Kill()
        $engineProcess.WaitForExit()
    }
    foreach ($name in $previous.Keys) { [Environment]::SetEnvironmentVariable($name, $previous[$name], 'Process') }
    $resolved = [IO.Path]::GetFullPath($probeHome)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if (-not $resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        -not (Split-Path -Leaf $resolved).StartsWith('kratos-turn-bench-')) { throw 'Unexpected benchmark cleanup path' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
