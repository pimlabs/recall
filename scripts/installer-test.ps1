#Requires -Version 5.1
<#
    install.ps1 against a release served from loopback: the Windows half of
    scripts/installer-test.sh, run by CI's `windows` job on both Windows
    architectures, under Windows PowerShell 5.1, which is what install.ps1
    is written for.

      powershell -NoProfile -ExecutionPolicy Bypass -File scripts\installer-test.ps1 target\debug\recall.exe

    <binary> is a real recall.exe (the `windows` job's own debug build), so
    what is installed can be run. It builds two releases around it:

      v0.4.6  recall-<rust target>.zip holding recall-<rust target>\recall.exe,
              packed by scripts/package-release.py, the script release.yml runs
      v0.4.5  recall_windows_<arch>.zip holding a bare recall.exe, as v0.4.5
              and older were packed

    plus v0.4.7, v0.4.6's archive with a checksums.txt that is wrong about
    it. scripts/fake-release-server.py serves them at GitHub's URLs, and
    install.ps1 is pointed there with RECALL_TEST_RELEASES_URL, then asked
    for "latest" on each side of the cutoff and for each version pinned.
    Each install is checked by the archive it says it downloaded and by
    running what it installed; the wrong checksum must install nothing.

    Loopback only, nothing written outside a temporary directory, and PATH
    left alone (RECALL_NO_PATH).
#>
param(
    [Parameter(Mandatory = $true)][string]$Binary
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$Binary = (Resolve-Path $Binary).Path
$installer = Join-Path $repo "install.ps1"
$packer = Join-Path $repo "scripts\package-release.py"

# Nothing from the calling shell may steer the installer anywhere real.
foreach ($name in @("RECALL_URL", "RECALL_TOKEN", "RECALL_PROJECT_KEY", "RECALL_AUTHKEY",
        "RECALL_WORKER_SERVER", "RECALL_VERSION", "RECALL_BIN_DIR", "RECALL_TEST_RELEASES_URL")) {
    Remove-Item "env:$name" -ErrorAction SilentlyContinue
}

$new = "0.4.6"  # the first release with the new names
$old = "0.4.5"  # the last release with the old ones

$archRaw = $env:PROCESSOR_ARCHITEW6432
if (-not $archRaw) { $archRaw = $env:PROCESSOR_ARCHITECTURE }
switch ($archRaw) {
    "AMD64" { $target = "x86_64-pc-windows-msvc"; $oldArch = "amd64" }
    "ARM64" { $target = "aarch64-pc-windows-msvc"; $oldArch = "arm64" }
    default { throw "installer-test.ps1: no Windows release archive for $archRaw" }
}
$newZip = "recall-$target.zip"
$oldZip = "recall_windows_$oldArch.zip"

$script:pass = 0
$script:fail = 0
function Check {
    param([string]$What, [bool]$Ok)
    if ($Ok) {
        $script:pass++
        Write-Host "ok    $What"
    } else {
        $script:fail++
        Write-Host "FAIL  $What"
    }
}

function Write-Checksums {
    # The format release.yml's `sha256sum *.tar.gz *.zip` writes: lowercase
    # hex, two spaces, the file name.
    param([string]$Dir)
    $lines = Get-ChildItem -Path $Dir -Filter *.zip | Sort-Object Name | ForEach-Object {
        (Get-FileHash -Path $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() + "  " + $_.Name
    }
    Set-Content -Path (Join-Path $Dir "checksums.txt") -Value $lines -Encoding Ascii
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("recall-installer-test-" + [System.Guid]::NewGuid().ToString("N"))
$root = Join-Path $work "root"
$server = $null

try {
    # ---- v0.4.6, packed by the script release.yml runs ----------------------
    $relNew = Join-Path $root "releases\download\v$new"
    New-Item -ItemType Directory -Force -Path $relNew | Out-Null
    & python $packer $Binary $newZip $relNew
    if ($LASTEXITCODE -ne 0) { throw "package-release.py failed" }
    Write-Checksums $relNew

    # ---- v0.4.5, a bare recall.exe, as release.yml packed it then -----------
    $relOld = Join-Path $root "releases\download\v$old"
    New-Item -ItemType Directory -Force -Path $relOld | Out-Null
    & python -c "import sys, zipfile; zipfile.ZipFile(sys.argv[1], 'w').write(sys.argv[2], 'recall.exe')" (Join-Path $relOld $oldZip) $Binary
    if ($LASTEXITCODE -ne 0) { throw "could not build the v$old zip" }
    Write-Checksums $relOld

    # ---- v0.4.7, v0.4.6's archive and a checksum that is wrong about it -----
    $relBad = Join-Path $root "releases\download\v0.4.7"
    New-Item -ItemType Directory -Force -Path $relBad | Out-Null
    Copy-Item (Join-Path $relNew $newZip) $relBad
    Set-Content -Path (Join-Path $relBad "checksums.txt") -Value (("0" * 64) + "  " + $newZip) -Encoding Ascii

    # ---- serve it -------------------------------------------------------------
    $latest = Join-Path $root "LATEST"
    $portFile = Join-Path $work "port"
    $server = Start-Process -FilePath "python" -PassThru -NoNewWindow `
        -ArgumentList @((Join-Path $repo "scripts\fake-release-server.py"), $root, $portFile) `
        -RedirectStandardError (Join-Path $work "server.log")
    for ($i = 0; $i -lt 100 -and -not (Test-Path $portFile); $i++) { Start-Sleep -Milliseconds 100 }
    Start-Sleep -Milliseconds 200
    $port = (Get-Content $portFile -Raw).Trim()
    $releases = "http://127.0.0.1:$port/releases"
    Write-Host "== serving $releases"

    # Install-Recall <name> [version]: install.ps1 in a fresh powershell.exe,
    # into $work\<name>. Returns its exit code; its output is $work\<name>.log.
    function Install-Recall {
        param([string]$Name, [string]$Version)
        $env:RECALL_TEST_RELEASES_URL = $releases
        $env:RECALL_BIN_DIR = Join-Path $work $Name
        $env:RECALL_NO_PATH = "1"
        if ($Version) { $env:RECALL_VERSION = $Version } else { Remove-Item env:RECALL_VERSION -ErrorAction SilentlyContinue }
        # Continue, not Stop, for the call: PowerShell 5.1 turns a native
        # command's stderr into error records, which Stop would throw on.
        $saved = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer *> (Join-Path $work "$Name.log")
        $code = $LASTEXITCODE
        $ErrorActionPreference = $saved
        Remove-Item env:RECALL_VERSION -ErrorAction SilentlyContinue
        return $code
    }

    function Test-Installed {
        # Runs the installed recall.exe; true when it says `recall <version>`.
        param([string]$Name)
        $exe = Join-Path (Join-Path $work $Name) "recall.exe"
        if (-not (Test-Path $exe)) { return $false }
        $out = & $exe version
        return ($LASTEXITCODE -eq 0 -and ($out -join "`n") -match '^recall \d+\.\d+\.\d+')
    }

    function Test-Log {
        param([string]$Name, [string]$Pattern)
        return [bool](Select-String -Path (Join-Path $work "$Name.log") -Pattern $Pattern -SimpleMatch -Quiet)
    }

    Write-Host "== install.ps1"
    Set-Content -Path $latest -Value $new -Encoding Ascii
    Check "latest ($new): installs" ((Install-Recall "latest-new") -eq 0)
    Check "latest ($new): downloaded $newZip" (Test-Log "latest-new" "downloading $newZip (v$new)")
    Check "latest ($new): recall.exe runs" (Test-Installed "latest-new")

    # Until 0.4.6 ships, latest is 0.4.5, and install.ps1 is served from
    # main: it has to find the old name for it.
    Set-Content -Path $latest -Value $old -Encoding Ascii
    Check "latest ($old): installs" ((Install-Recall "latest-old") -eq 0)
    Check "latest ($old): downloaded $oldZip" (Test-Log "latest-old" "downloading $oldZip (v$old)")
    Check "latest ($old): recall.exe runs" (Test-Installed "latest-old")

    Check "pinned v$new (latest $old): installs" ((Install-Recall "pin-new" "v$new") -eq 0)
    Check "pinned v${new}: downloaded $newZip" (Test-Log "pin-new" "downloading $newZip (v$new)")
    Check "pinned v${new}: recall.exe runs" (Test-Installed "pin-new")

    Set-Content -Path $latest -Value $new -Encoding Ascii
    Check "pinned $old, without the v (latest $new): installs" ((Install-Recall "pin-old" $old) -eq 0)
    Check "pinned ${old}: downloaded $oldZip" (Test-Log "pin-old" "downloading $oldZip (v$old)")
    Check "pinned ${old}: recall.exe runs" (Test-Installed "pin-old")

    Check "a wrong checksum is refused" ((Install-Recall "bad" "v0.4.7") -ne 0)
    Check "  ... saying so" (Test-Log "bad" "checksum verification failed")
    Check "  ... and installs nothing" (-not (Test-Path (Join-Path (Join-Path $work "bad") "recall.exe")))
} finally {
    if ($server) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
    Write-Host "passed $script:pass, failed $script:fail"
    if ($script:fail -ne 0) {
        Get-ChildItem -Path $work -Filter *.log -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "---- $($_.Name)"
            Get-Content $_.FullName | ForEach-Object { Write-Host $_ }
        }
    }
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($script:fail -ne 0 -or $script:pass -eq 0) { exit 1 }
