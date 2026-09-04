#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "tontinton/maki"
$Binary = "maki"
$InstallDir = if ($env:MAKI_INSTALL_DIR) {
    $env:MAKI_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "maki"
}

function Write-Err([string]$Message) {
    [Console]::Error.WriteLine("error: $Message")
    exit 1
}

function Get-GitHubHeaders {
    $headers = @{
        "User-Agent" = "maki-install"
        "Accept"     = "application/vnd.github+json"
    }
    $token = $env:GITHUB_TOKEN
    if (-not $token) {
        $token = $env:GH_TOKEN
    }
    if ($token) {
        $headers["Authorization"] = "Bearer $token"
    }
    return $headers
}

function Get-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch -Regex ($arch) {
        "^(AMD64|x86_64)$" { return "x86_64-pc-windows-msvc" }
        "^ARM64$" {
            # No native ARM64 release yet; x64 runs under emulation on Windows ARM.
            return "x86_64-pc-windows-msvc"
        }
        default { Write-Err "unsupported architecture: $arch" }
    }
}

function Get-LatestTag {
    $headers = Get-GitHubHeaders
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    } catch {
        Write-Err "failed to determine latest release tag: $_"
    }
    $tag = $release.tag_name
    if (-not $tag) {
        Write-Err "failed to determine latest release tag"
    }
    return $tag
}

function Install-Maki([string]$Tag) {
    $target = Get-Target
    if (-not $Tag) {
        $Tag = Get-LatestTag
    }

    $exeName = "$Binary.exe"
    $rawName = "$Binary-$Tag-$target-signed.exe"
    $rawUrl = "https://github.com/$Repo/releases/download/$Tag/$rawName"
    $archiveName = "$Binary-$Tag-$target.zip"
    $archiveUrl = "https://github.com/$Repo/releases/download/$Tag/$archiveName"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("maki-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        $src = $null
        Write-Host "downloading $Binary $Tag for $target (signed)..."
        $rawPath = Join-Path $tmp $exeName
        $downloadedRaw = $false
        try {
            Invoke-WebRequest -Uri $rawUrl -OutFile $rawPath -Headers (Get-GitHubHeaders) -ErrorAction Stop
            if ((Test-Path $rawPath) -and ((Get-Item $rawPath).Length -gt 0)) {
                $src = $rawPath
                $downloadedRaw = $true
                Write-Host "downloaded signed binary $rawName"
            }
        } catch {
            Write-Host "raw signed binary not found at $rawUrl, trying archive..."
        }
        if (-not $downloadedRaw) {
            $zipPath = Join-Path $tmp $archiveName
            Invoke-WebRequest -Uri $archiveUrl -OutFile $zipPath -Headers (Get-GitHubHeaders)
            Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
            $src = Join-Path $tmp $exeName
            if (-not (Test-Path -LiteralPath $src)) {
                Write-Err "archive did not contain $exeName"
            }
        }

        if (-not $src -or -not (Test-Path -LiteralPath $src)) {
            Write-Err "download failed: $exeName not found"
        }

        if (-not (Test-Path -LiteralPath $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        }

        $dest = Join-Path $InstallDir $exeName
        try {
            Move-Item -LiteralPath $src -Destination $dest -Force
        } catch {
            Write-Err "failed to install to $dest (try running as Administrator or set MAKI_INSTALL_DIR): $_"
        }

        Write-Host "$Binary $Tag installed to $dest"
        Add-ToUserPath -Dir $InstallDir
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Add-ToUserPath([string]$Dir) {
    $sep = [IO.Path]::PathSeparator
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($null -eq $userPath) {
        $userPath = ""
    }
    $entries = $userPath -split [regex]::Escape($sep) | Where-Object { $_ -ne "" }
    $already = $entries | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }
    if ($already) {
        return
    }

    $newPath = if ($userPath.Trim()) { "$userPath$sep$Dir" } else { $Dir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    $env:Path = "$env:Path$sep$Dir"
    Write-Host "added $Dir to user PATH (restart terminal if maki is not found)"
}

function Configure-Anchor([string]$AnchorUrl, [string]$AnchorName, [string]$AnchorToken) {
    if (-not $AnchorUrl) { return }
    if (-not $AnchorName) { $AnchorName = $env:COMPUTERNAME; if (-not $AnchorName) { $AnchorName = "maki" } }
    $configDir = Join-Path $env:APPDATA "maki"
    if (-not $configDir -or -not (Test-Path $env:APPDATA)) { $configDir = Join-Path $env:LOCALAPPDATA "maki-config" }
    # fallback to LOCALAPPDATA\maki like Linux XDG
    $altDir = Join-Path $env:LOCALAPPDATA "maki"
    $configDir = Join-Path $env:LOCALAPPDATA "maki"
    # use XDG-like: $env:APPDATA is Roaming, but maki uses LOCALAPPDATA on Windows for install, config is %APPDATA%\maki or %USERPROFILE%\.config\maki
    $configDir = if ($env:XDG_CONFIG_HOME) { Join-Path $env:XDG_CONFIG_HOME "maki" } else { Join-Path $env:APPDATA "maki" }
    if (-not (Test-Path $env:APPDATA)) { $configDir = Join-Path $HOME ".config/maki" }
    New-Item -ItemType Directory -Path $configDir -Force | Out-Null
    $initLua = Join-Path $configDir "init.lua"
    $tokenVal = if ($AnchorToken) { $AnchorToken } else { "YOUR_TOKEN_HERE" }
    $anchorBlock = @"
maki.setup {
  anchor = {
    url = "$AnchorUrl",
    name = "$AnchorName",
    token = "$tokenVal",
  },
}
"@
    if (-not (Test-Path $initLua)) {
        Set-Content -Path $initLua -Value $anchorBlock -Encoding utf8
        Write-Host "created $initLua with anchor $AnchorUrl (name $AnchorName)"
    } else {
        $existing = Get-Content $initLua -Raw -ErrorAction SilentlyContinue
        if ($existing -and $existing -match "anchor") {
            Write-Host "note: $initLua already contains anchor config; not modifying"
            Write-Host "  set url = `"$AnchorUrl`", name = `"$AnchorName`", token = `"$tokenVal`" manually if needed"
        } else {
            Add-Content -Path $initLua -Value "`n-- added by maki install --anchor`n$anchorBlock" -Encoding utf8
            Write-Host "appended anchor config to $initLua (name $AnchorName)"
        }
    }
    if ($tokenVal -eq "YOUR_TOKEN_HERE") {
        Write-Host "next: create a token on the anchor dashboard and set token in $initLua"
    }
}

# Parse args: supports --anchor, --name, --token, and positional tag
$AnchorUrl = $null; $AnchorName = $null; $AnchorToken = $null; $Tag = $null
$i = 0
while ($i -lt $args.Count) {
    switch ($args[$i]) {
        "--anchor" { $AnchorUrl = $args[$i+1]; $i += 2; continue }
        "--name" { $AnchorName = $args[$i+1]; $i += 2; continue }
        "--token" { $AnchorToken = $args[$i+1]; $i += 2; continue }
        "--help" { Write-Host "usage: install.ps1 [--anchor URL] [--name NAME] [--token TOKEN] [tag]"; exit 0 }
        "-h" { Write-Host "usage: install.ps1 [--anchor URL] [--name NAME] [--token TOKEN] [tag]"; exit 0 }
        "--" { $i++; break }
        default {
            if ($args[$i].StartsWith("-")) { Write-Err "unknown option $($args[$i])" }
            if (-not $Tag) { $Tag = $args[$i] } else { Write-Err "too many positional args: $($args[$i])" }
            $i++
        }
    }
}
Install-Maki -Tag $Tag
Configure-Anchor -AnchorUrl $AnchorUrl -AnchorName $AnchorName -AnchorToken $AnchorToken
