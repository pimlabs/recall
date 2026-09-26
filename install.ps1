#Requires -Version 5.1
<#
    Recall installer, for Windows machines:

      irm https://recall.pimlabs.id/install.ps1 | iex

    That URL is a Cloudflare Worker that fetches this file from main on every
    request — install-worker.js in this repository. It proxies rather than
    copies, so this file stays the only version of the installer, the same
    way install.sh is for macOS and Linux.

    Written against Windows PowerShell 5.1, which is what Windows 10 and 11
    ship, rather than PowerShell 7. `irm | iex` runs on a stock machine with
    nothing turned on, so this script cannot assume a newer runtime is there.
    That rules out the ternary operator and `??`; if you reach for one, test
    on 5.1 before it lands.

    Piped into `iex` there is no argv, so both knobs are environment
    variables, the same names install.sh already uses:

      $env:RECALL_VERSION = "v0.4.0"              # default: the latest release
      $env:RECALL_BIN_DIR = "C:\tools\recall"      # default: %LOCALAPPDATA%\recall\bin
      $env:RECALL_NO_PATH = "1"                    # do not touch PATH

    The rule that matters most, same as install.sh: never make a downloaded
    binary executable — or, on Windows, usable at all — before its SHA-256
    has been checked against the release's checksums.txt.

    Everything the script defines has to be defined before it is used: `iex`
    evaluates this text top to bottom, so a function declared below its
    caller does not exist yet when the caller runs.

    $env:RECALL_TEST_RELEASES_URL is for this repository's own CI and nothing
    else: it replaces https://github.com/pimlabs/recall/releases, so
    scripts/installer-test.ps1 can serve a release from loopback. The
    checksums come from the same place as the archive, so pointing it
    anywhere else verifies nothing. Leave it unset.
#>

$ErrorActionPreference = "Stop"

$Repo = "pimlabs/recall"
$Releases = $env:RECALL_TEST_RELEASES_URL
if (-not $Releases) { $Releases = "https://github.com/$Repo/releases" }

# v0.4.5 is the last release whose Windows archives are named
# recall_windows_<arch>.zip and hold a bare recall.exe. Every release after
# it names them recall-<rust target>.zip, holding a recall-<rust target>\
# directory with recall.exe in it. Tags never move, so both stay true for
# good.
$LastOldStyleRelease = [version]"0.4.5"

function Test-OldStyleArchive {
    # A pre-release counts as the version it leads up to.
    param([string]$Version)
    $core = ($Version.TrimStart('v') -split '[-+]')[0]
    return ([version]$core -le $LastOldStyleRelease)
}

function Write-Info {
    param([string]$Message)
    Write-Host "install.ps1: $Message"
}

function Stop-WithError {
    param([string]$Message)
    # `throw` rather than `exit`. Piped through `iex` this script runs in the
    # caller's own session, where `exit` closes their PowerShell window: a
    # failed install would take the terminal with it. A throw prints in red,
    # returns to the prompt, and still fails a CI step.
    throw "install.ps1: $Message"
}

function Publish-EnvironmentChange {
    <#
        Tell every running process that the environment changed. Without this
        the new PATH is invisible to everything already open, including the
        shell the user is standing in, and the install looks like it failed.
    #>
    if (-not ("RecallNative.Win32" -as [type])) {
        Add-Type -Namespace RecallNative -Name Win32 -MemberDefinition @"
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam,
    uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
"@
    }
    $HWND_BROADCAST = [IntPtr] 0xffff
    $WM_SETTINGCHANGE = 0x1a
    $SMTO_ABORTIFHUNG = 2
    $result = [UIntPtr]::Zero
    [RecallNative.Win32]::SendMessageTimeout(
        $HWND_BROADCAST, $WM_SETTINGCHANGE, [UIntPtr]::Zero,
        "Environment", $SMTO_ABORTIFHUNG, 5000, [ref] $result) | Out-Null
}

function Add-ToUserPath {
    param([string]$Directory)

    <#
        Written straight into HKCU:\Environment rather than through
        [Environment]::SetEnvironmentVariable. That API expands a REG_EXPAND_SZ
        PATH before writing it back, so an entry the user already had as
        %USERPROFILE%\... comes out as a literal path and stops following the
        variable it was written against. The registry API preserves the value
        kind, so read the kind and write the same one back.
    #>
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    if (-not $key) {
        Write-Info "warning: could not open HKCU:\Environment, so PATH was left alone. Add this directory yourself:"
        Write-Info "  $Directory"
        return
    }

    try {
        # DoNotExpandEnvironmentNames keeps %USERPROFILE% as written rather
        # than reading back a resolved copy we would then store.
        $current = $key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        try {
            $kind = $key.GetValueKind("Path")
        } catch {
            # No user PATH yet, which is unusual but legal.
            $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        }

        $entries = @($current -split ';' | Where-Object { $_ -ne "" })
        foreach ($entry in $entries) {
            if ($entry.TrimEnd('\') -ieq $Directory.TrimEnd('\')) {
                Write-Info "$Directory is already on your PATH."
                return
            }
        }

        $entries += $Directory
        $key.SetValue("Path", ($entries -join ';'), $kind)
        Write-Info "added $Directory to your user PATH."
    } finally {
        $key.Close()
    }

    Publish-EnvironmentChange

    # The broadcast above reaches processes that listen for it. This shell is
    # not one of them, so set it here too and the verification line below can
    # actually run.
    $env:PATH = "$env:PATH;$Directory"
}

function Test-GitBashAvailable {
    <#
        Claude Code runs hook commands through Git Bash on Windows, and falls
        back to PowerShell without it — which cannot run the bash-form hook
        command `recall init` writes (see docs/reference/install.md). Checked
        the same way `recall doctor` checks it: not verified against a real
        Windows Claude Code install, so this is the best-documented guess
        rather than a confirmed contract.
    #>
    if ($env:CLAUDE_CODE_GIT_BASH_PATH -and (Test-Path $env:CLAUDE_CODE_GIT_BASH_PATH -PathType Leaf)) {
        return $true
    }
    # bash.exe on PATH might be WSL's launcher (System32\bash.exe) rather
    # than Git for Windows' — same name, so only a path plainly naming a Git
    # installation is trusted.
    $onPath = Get-Command bash.exe -ErrorAction SilentlyContinue
    if ($onPath -and ($onPath.Source -match '(?i)\\Git\\')) {
        return $true
    }
    foreach ($candidate in @(
        "$env:ProgramFiles\Git\bin\bash.exe",
        "${env:ProgramFiles(x86)}\Git\bin\bash.exe"
    )) {
        if ($candidate -and (Test-Path $candidate -PathType Leaf)) {
            return $true
        }
    }
    return $false
}

# --- TLS ----------------------------------------------------------------
# Windows PowerShell 5.1 inherits the .NET Framework default, which on
# machines that have not been patched still negotiates TLS 1.0. GitHub refuses
# that, and the failure reads as "the underlying connection was closed", which
# names nothing a person can act on.
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # A newer runtime that no longer exposes the knob. Nothing to do.
}

# --- detect the architecture --------------------------------------------
# PROCESSOR_ARCHITECTURE describes the *process*, so a 32-bit PowerShell on
# 64-bit Windows reports x86 and would fetch the wrong archive.
# PROCESSOR_ARCHITEW6432 is set only in that case and describes the machine,
# so it wins when present.
$archRaw = $env:PROCESSOR_ARCHITEW6432
if (-not $archRaw) { $archRaw = $env:PROCESSOR_ARCHITECTURE }

switch ($archRaw) {
    "AMD64" { $arch = "amd64"; $target = "x86_64-pc-windows-msvc" }
    "ARM64" { $arch = "arm64"; $target = "aarch64-pc-windows-msvc" }
    default {
        Stop-WithError "this script does not support the architecture '$archRaw'. recall ships amd64 and arm64 builds for Windows; build from source instead: see docs/reference/install.md."
    }
}

# --- version and download URL --------------------------------------------
# The same two forms install.sh uses: the latest release by default, or a
# pinned tag. "latest" is resolved to a tag first, because the archive's
# name depends on which release it is: GitHub answers /releases/latest with
# a redirect to /releases/tag/<tag>, and that last path segment is the
# version. One HEAD request, via that redirect rather than the API, so
# there is nothing to rate-limit against.
$version = $env:RECALL_VERSION
if (-not $version) {
    try {
        $response = Invoke-WebRequest -Uri "$Releases/latest" -Method Head -UseBasicParsing
    } catch {
        Stop-WithError "could not look up the latest release at $Releases/latest: $($_.Exception.Message)"
    }
    # Where the redirect ended: HttpWebResponse.ResponseUri on Windows
    # PowerShell 5.1, the final request's URI on PowerShell 7.
    $final = $null
    if ($response.BaseResponse.ResponseUri) {
        $final = $response.BaseResponse.ResponseUri.AbsoluteUri
    } elseif ($response.BaseResponse.RequestMessage) {
        $final = $response.BaseResponse.RequestMessage.RequestUri.AbsoluteUri
    }
    if ($final) { $version = ($final.TrimEnd('/') -split '/')[-1] }
    if (-not $version) {
        Stop-WithError "could not tell which release is the latest from $Releases/latest. Pin one with `$env:RECALL_VERSION instead."
    }
    Write-Info "the latest release is $version"
}
$version = "v" + $version.TrimStart('v')
if ($version -notmatch '^v\d+\.\d+\.\d+') {
    Stop-WithError "not a release version: $version (expected something like v0.4.6)"
}
$baseUrl = "$Releases/download/$version"

if (Test-OldStyleArchive $version) {
    $archiveName = "recall_windows_${arch}.zip"
    $exeInArchive = "recall.exe"
} else {
    $archiveName = "recall-$target.zip"
    $exeInArchive = "recall-$target\recall.exe"
}

Write-Info "downloading $archiveName ($version)..."

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("recall-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    $archivePath = Join-Path $tmp $archiveName
    $checksumPath = Join-Path $tmp "checksums.txt"

    try {
        Invoke-WebRequest -Uri "$baseUrl/$archiveName" -OutFile $archivePath -UseBasicParsing
        Invoke-WebRequest -Uri "$baseUrl/checksums.txt" -OutFile $checksumPath -UseBasicParsing
    } catch {
        Stop-WithError "download failed: $($_.Exception.Message). If no release exists yet, build from source instead: git clone https://github.com/$Repo, then cargo build --release -p recall."
    }

    # --- verify the checksum ---------------------------------------------
    # checksums.txt lists every asset in the release, so filter it down to
    # the line naming this one before comparing.
    $expected = $null
    foreach ($line in Get-Content $checksumPath) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].Trim() -eq $archiveName) {
            $expected = $parts[0].Trim()
            break
        }
    }
    if (-not $expected) {
        Stop-WithError "checksums.txt has no line for $archiveName. The release may be incomplete; please report it at https://github.com/$Repo/issues."
    }

    # Get-FileHash returns uppercase and sha256sum (what the release
    # workflow runs) writes lowercase. PowerShell's -ne happens to be
    # case-insensitive, which is exactly the kind of thing a security check
    # should not lean on silently.
    $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash
    if ($actual.ToUpperInvariant() -cne $expected.ToUpperInvariant()) {
        Stop-WithError "checksum verification failed for $archiveName. The download may be corrupt or tampered with; the install stops here."
    }

    # --- unpack ------------------------------------------------------------
    $unpacked = Join-Path $tmp "unpacked"
    Expand-Archive -Path $archivePath -DestinationPath $unpacked -Force
    $exeSource = Join-Path $unpacked $exeInArchive
    if (-not (Test-Path $exeSource)) {
        Stop-WithError "the archive did not contain $exeInArchive. Please report it at https://github.com/$Repo/issues."
    }

    # --- choose the install directory ---------------------------------------
    # A per-user directory, the same as install.sh's default of
    # ~/.local/bin — no admin rights needed, and it does not collide with
    # anything a package manager might also manage.
    $installDir = $env:RECALL_BIN_DIR
    if (-not $installDir) {
        $installDir = Join-Path $env:LOCALAPPDATA "recall\bin"
    }
    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    $installPath = Join-Path $installDir "recall.exe"

    # Overwriting a running binary throws UnauthorizedAccessException, and the
    # raw message names nothing a person can act on. Name the process instead.
    if (Test-Path $installPath) {
        try {
            Remove-Item -Path $installPath -Force
        } catch {
            $running = Get-Process -Name "recall" -ErrorAction SilentlyContinue |
                Where-Object { $_.Path -eq $installPath }
            if ($running) {
                Stop-WithError "recall is already running (PID $(($running.Id) -join ', ')) and is holding $installPath open. Close it and run this again."
            }
            Stop-WithError "could not replace $($installPath): $($_.Exception.Message)"
        }
    }

    Copy-Item -Path $exeSource -Destination $installPath -Force

    # --- clear the mark of the web -------------------------------------------
    # The Windows half of what install.sh does with com.apple.quarantine.
    # Nothing here is code-signed, so a file carrying the zone identifier meets
    # a SmartScreen prompt on first run. Unblock-File is a no-op when the
    # stream is absent, which is the common case for a scripted download, so
    # that failure is expected and ignored.
    try {
        Unblock-File -Path $installPath -ErrorAction SilentlyContinue
    } catch {
    }

    Write-Info "installed $installPath"
    try {
        & $installPath --version | ForEach-Object { Write-Info $_ }
    } catch {
        # Not fatal — the file is in place and verified; a failure here says
        # something about running it, not about the install.
    }

    # --- PATH ---------------------------------------------------------------
    if ($env:RECALL_NO_PATH) {
        Write-Info "RECALL_NO_PATH is set, so PATH was left alone. Add this directory yourself:"
        Write-Info "  $installDir"
    } else {
        Add-ToUserPath -Directory $installDir
    }

    # --- Git for Windows ------------------------------------------------
    if (-not (Test-GitBashAvailable)) {
        Write-Info ""
        Write-Info "warning: Git for Windows was not found."
        Write-Info "  Claude Code runs hook commands through Git Bash on Windows, and falls"
        Write-Info "  back to PowerShell without it, which cannot run the hook command"
        Write-Info "  'recall init' writes. Install it: https://git-scm.com/download/win"
    }

    Write-Info ""
    Write-Info "Next: recall connect https://your-recall-host"
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
