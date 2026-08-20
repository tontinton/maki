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

    $archiveName = "$Binary-$Tag-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$Tag/$archiveName"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("maki-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        $zipPath = Join-Path $tmp $archiveName
        Write-Host "downloading $Binary $Tag for $target..."
        Invoke-WebRequest -Uri $url -OutFile $zipPath -Headers (Get-GitHubHeaders)

        Expand-Archive -Path $zipPath -DestinationPath $tmp -Force

        $exeName = "$Binary.exe"
        $src = Join-Path $tmp $exeName
        if (-not (Test-Path -LiteralPath $src)) {
            Write-Err "archive did not contain $exeName"
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

        # The bash tool needs a real bash, which almost always means Git for
        # Windows. The full lookup lives in maki-config, here we only decide
        # whether to offer the install, so a git on PATH is a good enough sign.
        if (-not (Get-Command git.exe -ErrorAction Ignore)) {
            Write-Host ""
            Write-Host "warning: git not found. The bash tool needs Git for Windows (or Cygwin, or MSYS2)."
            $wanted = (Test-WinGetAvailable) -and (Test-Interactive) -and
                (Read-Host "Install Git for Windows via winget? (y/N)") -imatch '^y'
            if ($wanted) {
                Write-Host "Installing Git for Windows..."
                winget install --id Git.Git -e --source winget
                if ($LASTEXITCODE -eq 0) {
                    Write-Host "Git for Windows installed. Restart your terminal to use bash."
                    return
                }
                Write-Host "winget failed (exit $LASTEXITCODE)."
            }
            Write-Host "To install Git for Windows:"
            Write-Host "  winget install --id Git.Git -e --source winget"
            Write-Host "  or download from https://git-scm.com/download/win"
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Test-WinGetAvailable {
    return $null -ne (Get-Command winget -ErrorAction Ignore)
}

# Read-Host is a terminating error in a non interactive host, and by the time
# we prompt maki is already installed, so a CI install must not fail there.
function Test-Interactive {
    return [Environment]::UserInteractive -and -not $env:CI -and
        -not [Console]::IsInputRedirected
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

$tag = if ($args.Count -ge 1) { $args[0] } else { $null }
Install-Maki -Tag $tag
