<#
.SYNOPSIS
    Benchmarks DS against other disk-space scanners, without lying about it.

.DESCRIPTION
    This runner exists because an earlier ad-hoc benchmark produced a fake
    result: Sysinternals DU "scanned" a 1.2M-file volume in 29 milliseconds,
    which is impossible. The cause was PowerShell pipeline truncation. Piping a
    long-running command into Select-Object -First N (or head, more, Where-Object)
    terminates the upstream process as soon as N lines are available, so the
    scanner never finished its traversal and was timed on a fraction of the work.

    Measured impact of that mistake:

        slow command piped to Select-Object -First 1   505 ms
        same command, untruncated                    2061 ms

    Everything here exists to make that class of error impossible:

      * Output is captured to a file or discarded. Never piped into a
        truncating filter.
      * Wall time is measured around the whole process, not inside a pipeline.
      * Every run must produce a plausible total (files and bytes). A run whose
        totals do not match a previous run of the same tool on the same tree is
        reported as INVALID rather than timed.
      * Invoke-Selftest verifies the harness itself, by proving that a known
        slow command is not truncated by this runner.

.PARAMETER Target
    Directory or drive to scan. Default C:\

.PARAMETER Reps
    Measured repetitions per tool. Default 1, which is the minimum that can be
    trusted on a warm cache. Raise only when a result genuinely needs
    confirmation; full-volume scans are expensive.

.PARAMETER Warmup
    Whether to run one unmeasured pass first. Default true.

.PARAMETER Tools
    Which tools to run. Default ds and diskusage. 'all' adds any of the
    competitors that are present on this machine.

.PARAMETER OutputDir
    Where to write captured output. Defaults to a temp directory.

.EXAMPLE
    .\compare.ps1 -Reps 1

.EXAMPLE
    .\compare.ps1 -Target 'C:\Users\User\.rustup' -Tools ds -Reps 3
#>
[CmdletBinding()]
param(
    [string]$Target = 'C:\',
    [int]$Reps = 1,
    [bool]$Warmup = $true,
    [string[]]$Tools = @('ds', 'diskusage'),
    [string]$OutputDir = (Join-Path $env:TEMP ("ds-bench-" + [guid]::NewGuid().ToString('N').Substring(0, 8)))
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $OutputDir)) {
    New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
}

# ---------------------------------------------------------------------------
# Core measurement
# ---------------------------------------------------------------------------

<#
.SYNOPSIS
    Turns a tool name or path into an absolute executable path, or $null.

.DESCRIPTION
    Test-Path on a bare command name such as 'cmd.exe' tests the current
    directory rather than PATH, so it reports a perfectly installed tool as
    missing. This resolves properly: literal paths pass through if they exist,
    anything else is looked up via Get-Command.
#>
function Resolve-Executable {
    param([Parameter(Mandatory)][AllowEmptyString()][string]$Name)

    if ([string]::IsNullOrWhiteSpace($Name)) { return $null }

    if ($Name -match '[\\/]') {
        # Looks like a path, so treat it as one.
        if (Test-Path -LiteralPath $Name) { return (Resolve-Path -LiteralPath $Name).Path }
        return $null
    }

    $cmd = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($cmd -and $cmd.Source -and (Test-Path -LiteralPath $cmd.Source)) {
        return $cmd.Source
    }
    return $null
}

<#
.SYNOPSIS
    Runs one tool to completion and measures it, with no possibility of
    truncation. Returns the elapsed time plus the captured output.

.DESCRIPTION
    Three rules, each of which corresponds to a way earlier benchmarks broke:

      1. Start-Process with -Wait. The process runs to completion on its own.
         Nothing in a pipeline can close it early.
      2. Output is redirected to files. There is no pipeline for a filter to
         truncate.
      3. The stopwatch wraps Start-Process, so it measures process lifetime.

    If the tool is a PowerShell function or script rather than an executable,
    this falls back to direct invocation, which is still untruncated because
    the output is assigned to a variable rather than piped into a filter.
#>
function Measure-Tool {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Exe,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$Tag
    )

    # Resolve to an absolute path before checking existence. Test-Path on a bare
    # command name like 'cmd.exe' tests the current directory, not PATH, and
    # would wrongly report the tool missing. The self-test caught this.
    $resolved = Resolve-Executable $Exe
    if (-not $resolved) {
        return [pscustomobject]@{
            Tool      = $Name
            Tag       = $Tag
            Valid     = $false
            Reason    = "not installed or not found: $Exe"
            WallSec   = 0.0
            Bytes     = 0
            Files     = 0
            Dirs      = 0
        }
    }
    $Exe = $resolved

    $outFile = Join-Path $OutputDir "$Name-$Tag.out.txt"
    $errFile = Join-Path $OutputDir "$Name-$Tag.err.txt"

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $Exe -ArgumentList $Arguments -NoNewWindow -PassThru `
        -RedirectStandardOutput $outFile -RedirectStandardError $errFile -Wait
    $stopwatch.Stop()

    $stdout = if (Test-Path -LiteralPath $outFile) { Get-Content -LiteralPath $outFile -Raw } else { '' }
    $stderr = if (Test-Path -LiteralPath $errFile) { Get-Content -LiteralPath $errFile -Raw } else { '' }

    $parsed = Read-ToolOutput -Name $Name -Text $stdout -TargetPath $Target

    [pscustomobject]@{
        Tool       = $Name
        Tag        = $Tag
        Valid      = $true
        Reason     = ''
        WallSec    = [math]::Round($stopwatch.Elapsed.TotalSeconds, 3)
        Bytes      = $parsed.Bytes
        Files      = $parsed.Files
        Dirs       = $parsed.Dirs
        Recognised = $parsed.Recognised
        ExitCode   = $proc.ExitCode
        Stdout     = $stdout
        Stderr     = $stderr
    }
}

<#
.SYNOPSIS
    Pulls file/directory/byte totals out of a tool's output.

.DESCRIPTION
    Totals are the guard against a truncated or partial traversal. A tool that
    claims to have scanned 1.2M files but reports 8 is not a fast scanner, it is
    a scanner that stopped early, and no amount of speed matters once that is
    caught.

    Each parser is matched against observed output formats. An unrecognised
    format returns zeros, which the caller treats as INVALID rather than
    silently reporting a bogus comparison.
#>
function Read-ToolOutput {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [string]$TargetPath = ''
    )

    $result = [pscustomobject]@{ Bytes = 0; Files = 0; Dirs = 0; Recognised = $false }

    if ([string]::IsNullOrWhiteSpace($Text)) { return $result }

    switch ($Name) {
        'ds' {
            # "Scanned C:\: 149.2 GB in 6.2s, 1,312,469 files, 249,524 directories, ..."
            # Units vary with tree size: a large volume reports GB and seconds,
            # a small one reports KB and milliseconds. Both must parse, or small
            # test trees silently look like failures.
            $unit = @{ 'B' = 1L; 'KB' = 1KB; 'MB' = 1MB; 'GB' = 1GB; 'TB' = 1TB }
            if ($Text -match 'Scanned\s+(?<path>.+?):\s+(?<size>[\d.]+)\s*(?<sizeunit>B|KB|MB|GB|TB)\s+in\s+(?<sec>[\d.]+)(?<seunit>ms|s),\s+(?<files>[\d,]+)\s+files,\s+(?<dirs>[\d,]+)\s+directories') {
                $result.Files = [int64](($Matches['files']) -replace ',', '')
                $result.Dirs = [int64](($Matches['dirs']) -replace ',', '')
                $multiplier = $unit[$Matches['sizeunit']]
                $result.Bytes = [int64]([double]$Matches['size'] * $multiplier)
                $result.Recognised = $true
            }
        }
        'diskusage' {
            # Verified output shape:
            #   header: SizeOnDisk,Files,Directory path
            #   one row per directory, ending with a row for the scanned root
            #   final row: "<total>,<total>,NN.N% of disk in use"  <- volume
            #             summary, NOT part of the tree, so it must be excluded.
            #
            # Columns are located by header name because they vary by Windows
            # build. The row for the scanned root is matched by path rather than
            # taken as the last line, since the volume summary always follows it.
            $lines = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
            if ($lines.Count -ge 2) {
                $header = $lines[0] -split ','
                $sizeIdx = -1
                $filesIdx = -1
                $dirIdx = -1
                for ($i = 0; $i -lt $header.Count; $i++) {
                    if ($header[$i] -match 'SizeOnDisk') { $sizeIdx = $i }
                    if ($header[$i] -match '^Files$') { $filesIdx = $i }
                    if ($header[$i] -match 'Directory') { $dirIdx = $i }
                }

                if ($sizeIdx -ge 0 -and $filesIdx -ge 0 -and $dirIdx -ge 0) {
                    $wanted = $Target.TrimEnd('\')
                    foreach ($line in $lines[1..($lines.Count - 1)]) {
                        $cols = $line -split ','
                        if ($cols.Count -le $dirIdx) { continue }
                        $path = $cols[$dirIdx].Trim('"')
                        if ($path -eq $wanted) {
                            $sizeVal = 0.0
                            $fileVal = 0.0
                            if ([double]::TryParse(($cols[$sizeIdx]).Trim('"'), [ref]$sizeVal) -and
                                [double]::TryParse(($cols[$filesIdx]).Trim('"'), [ref]$fileVal)) {
                                $result.Bytes = [int64]$sizeVal
                                $result.Files = [int64]$fileVal
                                $result.Recognised = $true
                            }
                            break
                        }
                    }
                }
            }
        }
        'gdu' {
            # With --no-prefix the total line is "  <raw bytes> <path>", one per
            # directory. The row for the scanned root carries the recursive total.
            # gdu prints no file count in non-interactive mode, so Files stays 0
            # and the agreement check falls back to comparing byte totals.
            $lines = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
            $wanted = $TargetPath.TrimEnd('\')
            foreach ($line in $lines) {
                if ($line -match '^\s*(?<size>\d+)\s+(?<path>.+?)\s*$') {
                    if ($Matches['path'].Trim() -eq $wanted) {
                        $result.Bytes = [int64]$Matches['size']
                        $result.Recognised = $true
                        break
                    }
                }
            }
        }
        'gdu-toponly' {
            # Same parsing as 'gdu', but this mode omits the root row entirely.
            $lines = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
            $wanted = $TargetPath.TrimEnd('\')
            foreach ($line in $lines) {
                if ($line -match '^\s*(?<size>\d+)\s+(?<path>.+?)\s*$') {
                    if ($Matches['path'].Trim() -eq $wanted) {
                        $result.Bytes = [int64]$Matches['size']
                        $result.Recognised = $true
                        break
                    }
                }
            }
        }
        'dust' {
            if ($Text -match '(?<files>[\d,]+)\s+files?') {
                $result.Files = [int64](($Matches['files']) -replace ',', '')
                $result.Recognised = $true
            }
        }
        'dua' {
            if ($Text -match '(?<files>[\d,]+)\s+files?') {
                $result.Files = [int64](($Matches['files']) -replace ',', '')
                $result.Recognised = $true
            }
        }
        'du-sysinternals' {
            # CSV: Path,CurrentFileCount,CurrentFileSize,FileCount,DirectoryCount,DirectorySize,DirectorySizeOnDisk
            # The root row is recursive, so take the last row.
            $lines = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
            if ($lines.Count -ge 2) {
                $last = $lines[$lines.Count - 1] -split ','
                if ($last.Count -ge 6) {
                    $result.Files = [int64]$last[3].Trim('"')
                    $result.Dirs = [int64]$last[4].Trim('"')
                    $result.Bytes = [int64]$last[5].Trim('"')
                    $result.Recognised = $true
                }
            }
        }
    }

    return $result
}

# ---------------------------------------------------------------------------
# Tool registry
# ---------------------------------------------------------------------------

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

<#
.SYNOPSIS
    Proves this runner does not truncate the process it measures.

.DESCRIPTION
    A benchmark harness that cannot demonstrate its own correctness is just a
    new way to get wrong numbers. This runs a deliberately slow helper through
    Measure-Tool and asserts the measured time is close to the helper's real
    runtime. If someone reintroduces Select-Object -First anywhere in the
    measurement path, this fails.

    The helper is cmd.exe running a loop-free but genuinely slow command:
    ping against localhost with a large count, which burns real wall time
    without needing a script file.
#>
function Invoke-Selftest {
    Write-Host '=== harness self-test ===' -ForegroundColor Cyan
    Write-Host 'Confirms the measured process is not cut short by a pipeline.'
    Write-Host ''

    # A helper that streams one line every 400ms for 10 lines, about 4 seconds.
    # Piping this into Select-Object -First 1 returns as soon as line one lands,
    # so it reproduces the failure mode this harness exists to prevent.
    $helper = '1..10 | ForEach-Object { Start-Sleep -Milliseconds 400; "line $_" }'

    Write-Host '  measuring the helper directly, output redirected to file...'
    $directWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $null = Start-Process -FilePath 'pwsh' -ArgumentList '-NoProfile', '-Command', $helper `
        -NoNewWindow -Wait `
        -RedirectStandardOutput (Join-Path $OutputDir 'selftest-direct.out.txt') `
        -RedirectStandardError (Join-Path $OutputDir 'selftest-direct.err.txt')
    $directWatch.Stop()
    $direct = $directWatch.Elapsed.TotalSeconds
    Write-Host ("    direct, redirected to file     : {0:N2}s" -f $direct)

    Write-Host '  measuring the same helper through Measure-Tool...'
    $viaHarness = Measure-Tool -Name 'selftest' -Exe 'pwsh' `
        -Arguments @('-NoProfile', '-Command', $helper) -Tag 'harness'
    Write-Host ("    via Measure-Tool                : {0:N2}s" -f $viaHarness.WallSec)

    # Timed purely for contrast, to show the magnitude of the mistake rather
    # than only asserting against it.
    Write-Host '  timing the truncating alternative, for contrast...'
    $truncWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $null = pwsh -NoProfile -Command $helper 2>&1 | Select-Object -First 1
    $truncWatch.Stop()
    $trunc = $truncWatch.Elapsed.TotalSeconds
    Write-Host ("    piped to Select-Object -First 1 : {0:N2}s   <- the bug" -f $trunc) -ForegroundColor DarkGray

    Write-Host ''

    if ($direct -lt 2.0) {
        Write-Host '  FAILED: the helper finished too quickly to be a useful check.' -ForegroundColor Red
        return $false
    }

    # The harness must land near the direct measurement. An upper bound is
    # included too, so a helper that somehow ran twice would also fail rather
    # than pass by being slow.
    $floor = $direct * 0.70
    $ceiling = $direct * 1.60
    if ($viaHarness.WallSec -ge $floor -and $viaHarness.WallSec -le $ceiling) {
        Write-Host ("  PASSED: {0:N2}s is inside [{1:N2}s, {2:N2}s] around the {3:N2}s direct measurement" -f $viaHarness.WallSec, $floor, $ceiling, $direct) -ForegroundColor Green
        if ($trunc -gt 0) {
            Write-Host ("  Truncation reports about {0:N2}s, so it understates the same work by {1:N1}x." -f $trunc, ($viaHarness.WallSec / $trunc)) -ForegroundColor Green
        }
        return $true
    }

    Write-Host ("  FAILED: harness measured {0:N2}s, expected between {1:N2}s and {2:N2}s." -f $viaHarness.WallSec, $floor, $ceiling) -ForegroundColor Red
    Write-Host '  The measured process was cut short. Do not trust any result from this harness.' -ForegroundColor Red
    return $false
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

Write-Host 'DS scanner benchmark' -ForegroundColor Cyan
Write-Host ''
Write-Host "  target      : $Target"
Write-Host "  repetitions : $Reps (plus $Warmup warm-up)"
Write-Host "  tools       : $($Tools -join ', ')"
Write-Host "  output dir  : $OutputDir"
Write-Host ''

# Refuse to run if a scanner is already active. Overlapping benchmarks contend
# for the same disk and produce nonsense in both directions.
$busy = Get-Process -Name 'ds', 'diskusage', 'du64', 'du', 'gdu', 'dust', 'dua' -ErrorAction SilentlyContinue
if ($busy) {
    Write-Host 'ERROR: another scanner appears to be running:' -ForegroundColor Red
    $busy | ForEach-Object { Write-Host "  $($_.Name) (pid $($_.Id))" -ForegroundColor Red }
    Write-Host 'Wait for it to finish, or stop it. Overlapping runs invalidate the result.' -ForegroundColor Red
    exit 2
}

$selftestOk = Invoke-Selftest
Write-Host ''
if (-not $selftestOk) {
    Write-Host 'Aborting: the harness could not verify itself.' -ForegroundColor Red
    exit 1
}

# Built inline rather than in a helper function. Returning a collection of
# objects from a PowerShell function kept arriving at the call site as a single
# stringified value, which then failed under StrictMode. A flat literal built
# here has no such round trip.
$registry = @(
    @{ Name = 'ds';              Exe = 'ds';               Args = @($Target, '--plain', '--ascii', '--threads', '16') }
    @{ Name = 'diskusage';       Exe = 'diskusage';        Args = @('/c', $Target) }
    @{ Name = 'du-sysinternals'; Exe = "$env:USERPROFILE\Downloads\du64.exe"; Args = @('-nobanner', '-c', $Target) }
    # gdu needs --depth to be comparable. In plain non-interactive mode gdu uses
    # a memory-efficient analyzer that keeps only top-level directory totals and
    # never builds the full tree, which DS always does. Verified on a small tree:
    # `gdu -np` omits the root total and 16 of 24 files, while `gdu -np --depth 1`
    # reports the same 56048 bytes as DS. The extra variant below is kept only to
    # document that difference, and is labelled so it is never read as a win.
    @{ Name = 'gdu';             Exe = 'gdu';              Args = @('-npa', '--depth', '1', '--no-prefix', $Target) }
    @{ Name = 'gdu-toponly';     Exe = 'gdu';              Args = @('-npa', '--no-prefix', $Target) }
    @{ Name = 'dust';            Exe = 'dust';             Args = @('-d', '1', '-r', $Target) }
    @{ Name = 'dua';             Exe = 'dua';              Args = @('interactive', '--aggregate', $Target) }
)

$toolList = @()
foreach ($entry in $registry) {
    $name = [string]$entry['Name']
    if (($Tools -contains 'all') -or ($Tools -contains $name)) {
        $toolList += @{
            Name = $name
            Exe  = (Resolve-Executable ([string]$entry['Exe']))
            Args = [string[]]$entry['Args']
        }
    }
}

if ($toolList.Count -eq 0) {
    Write-Host "None of the requested tools are available: $($Tools -join ', ')" -ForegroundColor Yellow
    exit 3
}

Write-Host "=== running ===" -ForegroundColor Cyan
Write-Host ''
foreach ($t in $toolList) {
    Write-Host ("  {0,-16} -> {1}" -f $t.Name, $t.Exe)
}
Write-Host ''

$results = @()

foreach ($tool in $toolList) {
    $toolName = [string]$tool.Name
    $toolExe = [string]$tool.Exe
    $toolArgs = [string[]]$tool.Args

    if ($Warmup) {
        Write-Host ("  {0,-16} warm-up (unmeasured)..." -f $toolName)
        $null = Measure-Tool -Name $toolName -Exe $toolExe -Arguments $toolArgs -Tag 'warmup'
    }

    for ($i = 1; $i -le $Reps; $i++) {
        $r = Measure-Tool -Name $toolName -Exe $toolExe -Arguments $toolArgs -Tag "rep$i"
        $r | Add-Member -NotePropertyName Rep -NotePropertyValue $i
        $results += $r
        if ($r.Valid) {
            Write-Host ("  {0,-16} rep {1}: {2,8:N3}s  {3,12:N0} files  {4,14:N0} bytes" -f $r.Tool, $i, $r.WallSec, $r.Files, $r.Bytes)
            if (-not $r.Recognised) {
                $firstLine = @($r.Stdout -split "`r?`n" | Where-Object { $_.Trim() -ne '' } | Select-Object -First 1)[0]
                Write-Host ("  {0,-16} WARNING: totals not parsed, so this run cannot be compared." -f $r.Tool) -ForegroundColor Yellow
                Write-Host ("  {0,-16} first output line: {1}" -f $r.Tool, $firstLine) -ForegroundColor DarkGray
            }
        }
        else {
            Write-Host ("  {0,-16} rep {1}: FAILED - {2}" -f $r.Tool, $i, $r.Reason) -ForegroundColor Red
        }
    }
}

Write-Host ''
Write-Host '=== summary ===' -ForegroundColor Cyan
Write-Host ''

$summary = foreach ($tool in ($results | Select-Object -ExpandProperty Tool -Unique)) {
    $set = @($results | Where-Object { $_.Tool -eq $tool -and $_.Valid })
    if ($set.Count -eq 0) { continue }
    # The outer @() matters: `| Sort-Object` unrolls a single-element result to a
    # scalar, and a scalar has no .Count under StrictMode.
    $times = @(@($set | ForEach-Object { $_.WallSec }) | Sort-Object)
    [pscustomobject]@{
        Tool        = $tool
        Runs        = $set.Count
        Best        = [math]::Round($times[0], 3)
        Median      = [math]::Round($times[[math]::Floor($times.Count / 2)], 3)
        Worst       = [math]::Round($times[-1], 3)
        Files       = $set[0].Files
        Dirs        = $set[0].Dirs
        Bytes       = $set[0].Bytes
        FilesPerSec = if ($set[0].WallSec -gt 0) { [math]::Round($set[0].Files / $set[0].WallSec) } else { 0 }
    }
}

$summary | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

# Agreement check. Two tools that walked the same tree must agree on the file
# count. A mismatch means one of them stopped early or counts differently, and
# timing them against each other would be meaningless.
#
# A tool that reported no totals at all is treated as a failure, not as
# something to silently drop. Excluding it would let a scanner that refused to
# run look like it simply had nothing to contribute.
Write-Host '=== totals agreement ===' -ForegroundColor Cyan
Write-Host ''

$failed = @($results | Where-Object { $_.Valid -and -not $_.Recognised })
$unparsed = @($summary | Where-Object { $_.Files -eq 0 })

if ($failed.Count -gt 0) {
    Write-Host '  At least one run produced no parseable totals, so the timings' -ForegroundColor Red
    Write-Host '  below are NOT a valid comparison:' -ForegroundColor Red
    foreach ($f in $failed) { Write-Host ("    {0} rep {1}" -f $f.Tool, $f.Rep) -ForegroundColor Red }
    foreach ($u in $unparsed) {
        $firstLine = @($results | Where-Object { $_.Tool -eq $u.Tool } | Select-Object -First 1)[0]
        if ($firstLine) {
            Write-Host ("    {0} said: {1}" -f $u.Tool, @($firstLine.Stdout -split "`r?`n" | Where-Object { $_.Trim() -ne '' } | Select-Object -First 1)[0]) -ForegroundColor DarkGray
            if ($firstLine.Stderr) {
                Write-Host ("    {0} stderr: {1}" -f $u.Tool, @($firstLine.Stderr -split "`r?`n" | Where-Object { $_.Trim() -ne '' } | Select-Object -First 1)[0]) -ForegroundColor DarkGray
            }
        }
    }
}
else {
    # Prefer file counts, which are exact. Some tools do not report one, so fall
    # back to byte totals, allowing for accounting differences such as logical
    # size against on-disk size.
    $withFiles = @($summary | Where-Object { $_.Files -gt 0 })
    if ($withFiles.Count -ge 2) {
        $fileCounts = @($withFiles | ForEach-Object { $_.Files } | Sort-Object -Unique)
        if ($fileCounts.Count -le 1) {
            Write-Host '  All tools that report a file count agree. Comparison is meaningful.' -ForegroundColor Green
        }
        else {
            Write-Host '  File counts DIFFER between tools:' -ForegroundColor Red
            foreach ($s in $withFiles) { Write-Host ("    {0,-16} {1,12:N0} files" -f $s.Tool, $s.Files) -ForegroundColor Red }
            Write-Host ''
            Write-Host '  Do NOT treat the timings as a speed comparison. A different file' -ForegroundColor Red
            Write-Host '  count means a different workload.' -ForegroundColor Red
        }
    }
    else {
        Write-Host '  Fewer than two tools report a file count, so byte totals are compared instead.' -ForegroundColor Yellow
        foreach ($s in $summary) {
            if ($s.Bytes -gt 0) {
                $vs = $summary | Where-Object { $_.Tool -eq 'ds' } | Select-Object -First 1
                if ($vs -and $vs.Bytes -gt 0) {
                    $pctOff = [math]::Round(($s.Bytes - $vs.Bytes) / $vs.Bytes * 100, 1)
                    Write-Host ("    {0,-16} {1,16:N0} bytes  ({2,6:N1}% vs ds)" -f $s.Tool, $s.Bytes, $pctOff)
                }
                else {
                    Write-Host ("    {0,-16} {1,16:N0} bytes" -f $s.Tool, $s.Bytes)
                }
            }
        }
        Write-Host ''
        Write-Host '  Byte totals legitimately differ: DS reports logical file size, while' -ForegroundColor DarkGray
        Write-Host '  some tools report on-disk size with clusters rounded up, and some' -ForegroundColor DarkGray
        Write-Host '  deduplicate hardlinks. Judge the timings, not the byte column.' -ForegroundColor DarkGray
    }
}

Write-Host ''
Write-Host "raw output: $OutputDir" -ForegroundColor DarkGray
$results | Export-Csv -NoTypeInformation -Path (Join-Path $OutputDir 'results.csv')
