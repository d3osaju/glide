<#
.SYNOPSIS
  Publish a new glide version. Every installed copy (with "Update automatically"
  on) picks it up within a few hours, or immediately via Settings > Check now.

.EXAMPLE
  .\release.ps1 -Version 0.3.1 -Notes "Fixes audio on Bluetooth headsets"
  .\release.ps1 -Version 0.4.0 -NotesFile notes.md

  Needs: gh (logged in), the signing key at $env:USERPROFILE\.glide\release-signing.key
#>
param(
    [Parameter(Mandatory)] [string]$Version,
    [string]$Notes,
    [string]$NotesFile,
    [string]$Key = (Join-Path $env:USERPROFILE ".glide\release-signing.key")
)
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3" }
if (-not (Test-Path $Key)) { throw "Signing key not found at $Key" }
if (git status --porcelain) { throw "Commit or stash your changes first" }
$tag = "v$Version"
if (git tag -l $tag) { throw "$tag already exists" }

# 1. Bump version
(Get-Content Cargo.toml -Raw) -replace '(?m)^version = "[^"]+"', "version = `"$Version`"" | Set-Content -NoNewline Cargo.toml
(Get-Content tauri.conf.json -Raw) -replace '"version":\s*"[^"]+"', "`"version`": `"$Version`"" | Set-Content -NoNewline tauri.conf.json

# 2. Check the UI script parses (a syntax error there blanks the whole app)
$js = [regex]::Match((Get-Content ui\index.html -Raw), '(?s)<script>(.*)</script>').Groups[1].Value
$tmp = Join-Path $env:TEMP "glide-ui-check.js"
Set-Content -Encoding utf8 $tmp $js
node --check $tmp
if ($LASTEXITCODE -ne 0) { git checkout -- Cargo.toml tauri.conf.json; throw "ui/index.html has a JavaScript syntax error" }

# 3. Test + build
$tc = (rustc --print sysroot).Trim()
$env:RUSTC = Join-Path $tc "bin\rustc.exe"
$cargo = Join-Path $tc "bin\cargo.exe"
& $cargo test --release
if ($LASTEXITCODE -ne 0) { git checkout -- Cargo.toml tauri.conf.json; throw "tests failed" }
& .\build-dist.ps1

# 4. Sign (the app refuses unsigned or mis-signed updates)
# glide.exe is a GUI-subsystem binary, so PowerShell must be told to wait for it.
$p = Start-Process -FilePath dist\glide.exe -ArgumentList @("release-sign", "dist\glide.exe", "--key", "`"$Key`"") -Wait -NoNewWindow -PassThru
if ($p.ExitCode -ne 0 -or -not (Test-Path dist\glide.exe.sig)) { throw "signing failed" }

# 5. Commit, tag, push, publish
git add Cargo.toml Cargo.lock tauri.conf.json
git commit -m "Release $tag"
git tag $tag
git push
git push origin $tag

$notesArgs = if ($NotesFile) { @("--notes-file", $NotesFile) } elseif ($Notes) { @("--notes", $Notes) } else { @("--generate-notes") }
gh release create $tag dist\glide-windows-x64.zip dist\glide.exe dist\glide.exe.sig --title "glide $tag" --latest @notesArgs
Write-Host "Published $tag. Installed copies will update themselves." -ForegroundColor Green
