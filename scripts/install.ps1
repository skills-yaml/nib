param(
    [ValidateSet("prod", "production", "dev", "development")]
    [string] $Channel = "prod",
    [string] $Repo = "example/nib",
    [string] $InstallDir = "$env:USERPROFILE\.local\bin",
    [switch] $AddToPath
)

$ErrorActionPreference = "Stop"

switch ($Channel) {
    "prod" { $Tag = "prod-latest" }
    "production" { $Tag = "prod-latest" }
    "dev" { $Tag = "development-latest" }
    "development" { $Tag = "development-latest" }
}

$arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($arch -ne "X64") {
    throw "Unsupported Windows architecture: $arch"
}

$Asset = "nib-windows-x86_64.zip"
$Url = "https://github.com/$Repo/releases/download/$Tag/$Asset"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "nib-install-$([System.Guid]::NewGuid())"

New-Item -ItemType Directory -Force -Path $TempDir | Out-Null
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

try {
    $Archive = Join-Path $TempDir $Asset
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Archive
    Expand-Archive -Path $Archive -DestinationPath $TempDir -Force
    Copy-Item (Join-Path $TempDir "nib.exe") (Join-Path $InstallDir "nib.exe") -Force

    if ($AddToPath) {
        $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $PathEntries = @()
        if ($UserPath) {
            $PathEntries = $UserPath -split ";"
        }

        if ($PathEntries -notcontains $InstallDir) {
            $NewPath = if ($UserPath) { "$UserPath;$InstallDir" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
            Write-Host "Added $InstallDir to the user PATH. Open a new terminal to use it."
        }
    }

    $InstalledBinary = Join-Path $InstallDir "nib.exe"
    Write-Host "Installed nib to $InstalledBinary"
}
finally {
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
