# lumux installer — Windows (PowerShell).
#
#   irm https://hgaol.github.io/lumux/install.ps1 | iex
#
# Downloads the latest lumux release for Windows (x64) from GitHub, verifies
# its SHA256 against the published .sha256 sidecar, and installs lumux.exe to
# %LOCALAPPDATA%\lumux\bin (override with $env:LUMUX_INSTALL_DIR), adding that
# directory to your user PATH.
$ErrorActionPreference = 'Stop'

$repo   = 'hgaol/lumux'
$target = 'x86_64-pc-windows-msvc'
$dir    = if ($env:LUMUX_INSTALL_DIR) { $env:LUMUX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'lumux\bin' }

function Info($m) { Write-Host "  $m" }
function Say($m)  { Write-Host $m -ForegroundColor Green }

# --- resolve latest version tag --------------------------------------------
Say 'Resolving the latest lumux release...'
$rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'lumux-install' }
$tag = $rel.tag_name
if (-not $tag) { throw 'could not determine the latest release tag' }
$version = $tag.TrimStart('v')
Info "latest is $tag for $target"

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

  # --- unpack + install ----------------------------------------------------
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $exe = Get-ChildItem -Path $tmp -Recurse -Filter 'lumux.exe' | Select-Object -First 1
  if (-not $exe) { throw 'could not find lumux.exe inside the archive' }

  New-Item -ItemType Directory -Path $dir -Force | Out-Null
  Copy-Item -Path $exe.FullName -Destination (Join-Path $dir 'lumux.exe') -Force
  Say "Installed lumux $version -> $dir\lumux.exe"

  # --- add to user PATH ----------------------------------------------------
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notmatch [Regex]::Escape($dir)) {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$dir", 'User')
    Write-Host "note: added $dir to your user PATH - open a new terminal to pick it up." -ForegroundColor Yellow
  }
}
finally {
  Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host 'Start a session:  ' -NoNewline
Write-Host 'lumux new -s work' -ForegroundColor Green -NoNewline
Write-Host '   (tip: Set-Alias lm lumux)'
