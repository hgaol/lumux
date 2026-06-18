# lumux installer — Windows (PowerShell).
#
#   irm https://hgaol.github.io/lumux/scripts/install.ps1 | iex
#
# Downloads the latest lumux release for Windows (x64) from GitHub, verifies
# its SHA256 against the published .sha256 sidecar, and installs lumux.exe to
# %LOCALAPPDATA%\lumux\bin (override with $env:LUMUX_INSTALL_DIR), adding that
# directory to your user PATH. Re-run it to upgrade — it replaces a running
# lumux.exe safely (see the rename-aside note below).
$ErrorActionPreference = 'Stop'

$repo   = 'hgaol/lumux'
$target = 'x86_64-pc-windows-msvc'
$dir    = if ($env:LUMUX_INSTALL_DIR) { $env:LUMUX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'lumux\bin' }
$dest   = Join-Path $dir 'lumux.exe'

function Info($m) { Write-Host "  $m" }
function Say($m)  { Write-Host $m -ForegroundColor Green }
function Warn($m) { Write-Host $m -ForegroundColor Yellow }

# --- resolve latest version tag --------------------------------------------
Say 'Resolving the latest lumux release...'
$rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'lumux-install' }
$tag = $rel.tag_name
if (-not $tag) { throw 'could not determine the latest release tag' }
$version = $tag.TrimStart('v')
Info "latest is $tag for $target"

# --- already up to date? ---------------------------------------------------
# If lumux.exe is already at the latest version, don't bother downloading.
if (Test-Path $dest) {
  try {
    $have = (& $dest --version 2>$null) -replace '^lumux\s+', ''
    if ($have.Trim() -eq $version) {
      Say "lumux $version is already installed at $dest - nothing to do."
      return
    }
    Info "installed: $have  ->  updating to $version"
  } catch {
    Info "found an existing lumux.exe (version unknown) - updating to $version"
  }
}

$asset = "lumux-$tag-$target.zip"
$base  = "https://github.com/$repo/releases/download/$tag"

# --- download + verify in a temp dir ---------------------------------------
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("lumux-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  Say "Downloading $asset..."
  $zip = Join-Path $tmp $asset
  Invoke-WebRequest -Uri "$base/$asset"        -OutFile $zip          -UseBasicParsing
  Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile "$zip.sha256" -UseBasicParsing

  Say 'Verifying checksum...'
  $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
  $actual   = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
  if ($expected -ne $actual) { throw "checksum mismatch - expected $expected, got $actual" }
  Info "ok ($actual)"

  # --- unpack --------------------------------------------------------------
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exe = Get-ChildItem -Path $tmp -Recurse -Filter 'lumux.exe' | Select-Object -First 1
  if (-not $exe) { throw 'could not find lumux.exe inside the archive' }

  New-Item -ItemType Directory -Path $dir -Force | Out-Null

  # --- install / replace ---------------------------------------------------
  # Windows locks a running .exe against in-place overwrite (Copy-Item -Force
  # fails with "being used by another process" if a lumux server or client is
  # live). But it *does* allow renaming the running image. So move the old exe
  # aside, then drop the new one into the freed path; delete the stale copy
  # afterward (it'll be unlocked once that old process exits).
  if (Test-Path $dest) {
    $old = "$dest.old"
    Remove-Item $old -Force -ErrorAction SilentlyContinue
    try {
      Move-Item -Path $dest -Destination $old -Force
    } catch {
      throw "could not move the existing lumux.exe aside (is it locked?). Close lumux and retry. Underlying error: $($_.Exception.Message)"
    }
    Move-Item -Path $exe.FullName -Destination $dest -Force
    Remove-Item $old -Force -ErrorAction SilentlyContinue   # ok if still locked; cleaned next run
  } else {
    Move-Item -Path $exe.FullName -Destination $dest -Force
  }
  Say "Installed lumux $version -> $dest"

  # --- verify what's actually on disk now ----------------------------------
  try {
    $now = (& $dest --version 2>$null).Trim()
    Info "verified: $now"
  } catch {
    Warn "installed, but could not run '$dest --version' to verify."
  }

  # --- add to user PATH ----------------------------------------------------
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notmatch [Regex]::Escape($dir)) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
    Warn "note: added $dir to your user PATH - open a new terminal to pick it up."
  }

  # --- warn if a *different* lumux.exe shadows ours on PATH ----------------
  # A common "I updated but the version didn't change" cause: another lumux.exe
  # (e.g. from cargo install, in ~\.cargo\bin) comes earlier on PATH.
  $found = Get-Command lumux.exe -ErrorAction SilentlyContinue
  if ($found -and $found.Source -and ($found.Source -ne $dest)) {
    Warn "note: another lumux.exe is ahead of this one on PATH:"
    Warn "        $($found.Source)"
    Warn "      That one will run instead. Remove it or reorder PATH so $dir wins."
  }
}
finally {
  Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Warn 'If a lumux server was already running, it keeps the old version until restarted:'
Write-Host '    lumux kill-server' -ForegroundColor Green -NoNewline
Write-Host '   then start fresh.'
Write-Host 'Start a session:  ' -NoNewline
Write-Host 'lumux new -s work' -ForegroundColor Green -NoNewline
Write-Host '   (tip: Set-Alias lm lumux)'
