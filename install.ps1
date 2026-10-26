<#
.SYNOPSIS
Installs rtk-mcp for Claude Desktop on Windows.

.DESCRIPTION
Downloads the Windows build of rtk-mcp from its GitHub release and checks it against the release's
checksums.txt. If RTK (Rust Token Killer, the `rtk` command) is not installed yet, downloads it too,
next to rtk-mcp.exe, with the same check. Then adds rtk-mcp to Claude Desktop's
claude_desktop_config.json: other servers in that file are kept, and the file is backed up first.

Everything goes under your user profile; no administrator rights are needed.

.PARAMETER InstallDir
Where rtk-mcp.exe (and rtk.exe) go. Default: %LOCALAPPDATA%\Programs\rtk-mcp

.PARAMETER Version
The rtk-mcp release to install, such as v0.6.0. Default: latest.

.PARAMETER RtkVersion
The RTK release to install when RTK is missing, such as v1.1.0 (the default, the newest one
rtk-mcp is tested with). 'latest' takes the newest.

.PARAMETER RtkEdition
Which RTK build to install when RTK is missing: 'full' (the default) is RTK's standard build, 'oss'
is the Apache-2.0 build that RTK publishes from 1.0.0 on. rtk-mcp works with both.

.PARAMETER NoConfig
Install only; leave Claude Desktop's configuration alone.

.PARAMETER ConfigPath
Edit this claude_desktop_config.json instead of looking for Claude Desktop's.

.EXAMPLE
irm https://raw.githubusercontent.com/admiralhr99/rtk-mcp/main/install.ps1 | iex

.EXAMPLE
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/admiralhr99/rtk-mcp/main/install.ps1))) -Version v0.6.0 -NoConfig
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\rtk-mcp'),
    [string]$Version = 'latest',
    [string]$RtkVersion = 'v1.1.0',
    [ValidateSet('full', 'oss')][string]$RtkEdition = 'full',
    [switch]$NoConfig,
    [string]$ConfigPath
)

# This file is plain ASCII on purpose: Windows PowerShell 5.1 reads a script without a byte order
# mark in the ANSI code page.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # the progress bar makes Invoke-WebRequest very slow in 5.1

$Repo = 'admiralhr99/rtk-mcp'
$ServerAsset = 'rtk-mcp-x86_64-pc-windows-msvc.zip'
$RtkAsset = if ($RtkEdition -eq 'oss') { 'rtk-oss-x86_64-pc-windows-msvc.zip' } else { 'rtk-x86_64-pc-windows-msvc.zip' }

function Get-ReleaseBase([string]$Project, [string]$Tag) {
    if ($Tag -eq 'latest') { return "https://github.com/$Project/releases/latest/download" }
    if ($Tag -notmatch '^v') { $Tag = "v$Tag" }
    return "https://github.com/$Project/releases/download/$Tag"
}

# Downloads $Asset and checksums.txt from $Base into $Dir. A file that does not match its entry in
# checksums.txt is refused. Returns the path of the downloaded file.
function Get-VerifiedAsset([string]$Base, [string]$Asset, [string]$Dir) {
    $file = Join-Path $Dir $Asset
    $sums = Join-Path $Dir ('checksums-' + [guid]::NewGuid().ToString('N') + '.txt')
    try {
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile $file
        Invoke-WebRequest -UseBasicParsing -Uri "$Base/checksums.txt" -OutFile $sums
    } catch {
        throw "Could not download $Base/$Asset ($($_.Exception.Message)). Is there a release with a Windows build at that address?"
    }
    $expected = $null
    foreach ($line in Get-Content -Path $sums) {
        if ($line -match ('^\s*([0-9a-fA-F]{64})\s+\*?' + [regex]::Escape($Asset) + '\s*$')) {
            $expected = $Matches[1].ToLowerInvariant()
            break
        }
    }
    if (-not $expected) { throw "checksums.txt lists no entry for $Asset, so it was not installed." }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $file).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA-256 of $Asset is $actual but the release lists $expected, so it was not installed."
    }
    return $file
}

# Puts the file called $Name from the archive into $InstallDir. A copy that is running (Claude
# Desktop has it open) cannot be overwritten, but it can be renamed, so the old one is moved aside.
function Install-FromArchive([string]$Archive, [string]$Name) {
    $stage = Join-Path ([IO.Path]::GetDirectoryName($Archive)) ([IO.Path]::GetFileNameWithoutExtension($Archive))
    Expand-Archive -Path $Archive -DestinationPath $stage -Force
    $found = Get-ChildItem -Path $stage -Recurse -Filter $Name -File | Select-Object -First 1
    if (-not $found) { throw "$Name is not in $([IO.Path]::GetFileName($Archive))." }
    $target = Join-Path $InstallDir $Name
    Get-ChildItem -Path $InstallDir -Filter "$Name.*.old" -File -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $target) {
        $aside = "$target." + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.old'
        Move-Item -LiteralPath $target -Destination $aside -Force
    }
    Copy-Item -LiteralPath $found.FullName -Destination $target -Force
    Unblock-File -LiteralPath $target
    return $target
}

# Is $Path a working RTK? The same two questions rtk-mcp asks when it starts.
function Test-Rtk([string]$Path) {
    try {
        $first = & $Path --version 2>$null | Select-Object -First 1
        if (-not ("$first" -like 'rtk *')) { return $false }
        & $Path rewrite -- 'git status' 2>$null | Out-Null
        return ($LASTEXITCODE -eq 0 -or $LASTEXITCODE -eq 2 -or $LASTEXITCODE -eq 3)
    } catch {
        return $false
    }
}

# Claude Desktop's config file. The Microsoft Store build keeps its files in its package folder
# instead of %APPDATA%; whichever file already exists wins, the Store one first.
function Find-ClaudeConfig {
    $classic = Join-Path $env:APPDATA 'Claude\claude_desktop_config.json'
    $candidates = @()
    $packages = Join-Path $env:LOCALAPPDATA 'Packages'
    if (Test-Path -LiteralPath $packages) {
        $candidates += @(
            Get-ChildItem -LiteralPath $packages -Directory -Filter 'Claude_*' -ErrorAction SilentlyContinue |
                ForEach-Object { Join-Path $_.FullName 'LocalCache\Roaming\Claude\claude_desktop_config.json' }
        )
    }
    $candidates += $classic
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath (Split-Path -Parent $candidate)) { return $candidate }
    }
    return $classic
}

# Adds (or updates) mcpServers.rtk in the config file, keeping everything else in it.
function Set-ClaudeConfig([string]$Path, [string]$Command) {
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    $config = $null
    if (Test-Path -LiteralPath $Path) {
        $raw = [IO.File]::ReadAllText($Path)
        if ($raw.Trim().Length -gt 0) {
            try {
                $config = $raw | ConvertFrom-Json
            } catch {
                throw "$Path is not valid JSON ($($_.Exception.Message)), so it was left alone. Add an `"rtk`" entry with `"command`": `"$($Command.Replace('\', '\\'))`" under mcpServers by hand."
            }
            if ($config -isnot [System.Management.Automation.PSCustomObject]) {
                throw "$Path does not hold a JSON object, so it was left alone."
            }
        }
        $backup = "$Path.bak-" + (Get-Date -Format 'yyyyMMddHHmmss')
        Copy-Item -LiteralPath $Path -Destination $backup
        Write-Host "  Backup of the old config: $backup"
    }
    if ($null -eq $config) { $config = New-Object psobject }
    if (($null -eq $config.PSObject.Properties['mcpServers']) -or ($null -eq $config.mcpServers)) {
        Add-Member -InputObject $config -NotePropertyName 'mcpServers' -NotePropertyValue (New-Object psobject) -Force
    }
    $servers = $config.mcpServers
    $entry = $servers.PSObject.Properties['rtk']
    if ($entry -and ($entry.Value -is [System.Management.Automation.PSCustomObject])) {
        # an entry the user already has: change where it points, keep its env and the rest
        Add-Member -InputObject $entry.Value -NotePropertyName 'command' -NotePropertyValue $Command -Force
    } else {
        Add-Member -InputObject $servers -NotePropertyName 'rtk' -NotePropertyValue ([pscustomobject]@{ command = $Command }) -Force
    }
    $json = $config | ConvertTo-Json -Depth 100
    [IO.File]::WriteAllText($Path, $json + [Environment]::NewLine, (New-Object System.Text.UTF8Encoding($false)))
}

function Test-GitBash {
    foreach ($root in @($env:ProgramFiles, $env:ProgramW6432, ${env:ProgramFiles(x86)}, (Join-Path $env:LOCALAPPDATA 'Programs'))) {
        if ($root -and (Test-Path -LiteralPath (Join-Path $root 'Git\bin\bash.exe'))) { return $true }
    }
    return [bool](Get-Command git -CommandType Application -ErrorAction SilentlyContinue)
}

if ($env:OS -ne 'Windows_NT') { throw 'This installer is for Windows. On Linux and macOS use install.sh.' }
if (-not [Environment]::Is64BitOperatingSystem) { throw 'rtk-mcp needs 64-bit Windows.' }
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$InstallDir = [IO.Path]::GetFullPath($InstallDir)
$work = Join-Path ([IO.Path]::GetTempPath()) ('rtk-mcp-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    Write-Host "Installing rtk-mcp ($Version) ..."
    $archive = Get-VerifiedAsset (Get-ReleaseBase $Repo $Version) $ServerAsset $work
    $server = Install-FromArchive $archive 'rtk-mcp.exe'
    Write-Host "  rtk-mcp: $server (SHA-256 verified)"

    $rtk = Get-Command rtk -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($rtk -and (Test-Rtk $rtk.Source)) {
        Write-Host "  RTK: already installed ($($rtk.Source))"
    } else {
        if ($RtkEdition -eq 'oss' -and $RtkVersion -match '^v?0\.') {
            throw "RTK publishes the oss build from 1.0.0 on, so $RtkVersion has none. Use -RtkVersion v1.1.0 or -RtkEdition full."
        }
        Write-Host "Installing RTK ($RtkVersion, $RtkEdition) next to rtk-mcp ..."
        $archive = Get-VerifiedAsset (Get-ReleaseBase 'rtk-ai/rtk' $RtkVersion) $RtkAsset $work
        $bundled = Install-FromArchive $archive 'rtk.exe'
        if (-not (Test-Rtk $bundled)) { throw "$bundled does not run as RTK." }
        Write-Host "  RTK: $bundled (SHA-256 verified)"
        Write-Host '  RTK is a separate project with its own licence: https://github.com/rtk-ai/rtk (-RtkEdition oss installs its Apache-2.0 build).'
    }

    if ($NoConfig) {
        Write-Host 'Claude Desktop configuration left alone (-NoConfig).'
    } else {
        if (-not $ConfigPath) { $ConfigPath = Find-ClaudeConfig }
        Set-ClaudeConfig $ConfigPath $server
        Write-Host "  Claude Desktop config: $ConfigPath"
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host 'Done.'
if (-not $NoConfig) {
    Write-Host 'Quit Claude Desktop completely (right-click its tray icon, Quit), start it again, and ask it to run `dir`.'
}
if (-not (Test-GitBash)) {
    Write-Host ''
    Write-Host 'Git for Windows was not found, so commands run under cmd.exe (dir, type, findstr).'
    Write-Host 'Install it from https://gitforwindows.org to get ls, grep, pipes and the rest of the POSIX syntax.'
}
