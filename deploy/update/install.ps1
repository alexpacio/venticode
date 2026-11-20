# Venticode installer for Windows (PowerShell):
#   irm <release-url>/install.ps1 | iex
# $env:VENTICODE_COMPONENTS ("tui,harness" by default) and $env:VENTICODE_INSTALL_DIR
# override the defaults. publish.sh stamps the two placeholders below.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$ManifestUrl = '@MANIFEST_URL@'
$PubKey = '@PUBKEY@'
if ($ManifestUrl -notmatch '^(https://|http://(127\.0\.0\.1|localhost))') {
    throw 'This installer was not stamped with an https manifest URL (use the published install.ps1).'
}
$dir = if ($env:VENTICODE_INSTALL_DIR) { $env:VENTICODE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\venticode' }
$components = if ($env:VENTICODE_COMPONENTS) { $env:VENTICODE_COMPONENTS -split ',' } else { 'tui', 'harness' }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $manifestFile = Join-Path $tmp 'manifest.json'
    Invoke-WebRequest -UseBasicParsing $ManifestUrl -OutFile $manifestFile
    if (Get-Command minisign -ErrorAction SilentlyContinue) {
        Invoke-WebRequest -UseBasicParsing "$ManifestUrl.minisig" -OutFile "$manifestFile.minisig"
        minisign -Vm $manifestFile -P $PubKey -x "$manifestFile.minisig" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'The manifest signature does not verify.' }
    } else {
        Write-Warning 'minisign not found: trusting HTTPS and the sha256 for this install (later updates are signature-checked).'
    }
    $manifest = Get-Content -Raw $manifestFile | ConvertFrom-Json
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    foreach ($component in $components) {
        if ($component -notin 'tui', 'harness') { throw "Unknown component $component (use tui or harness)." }
        $name = "venticode-$component"
        $entry = $manifest.artifacts.'windows-x64'.$name
        if (-not $entry) { throw "$name is not published for windows-x64." }
        $file = Join-Path $tmp "$name.exe"
        Invoke-WebRequest -UseBasicParsing ([Uri]::new([Uri]$ManifestUrl, $entry.url).AbsoluteUri) -OutFile $file
        if ((Get-FileHash $file -Algorithm SHA256).Hash -ne $entry.sha256) { throw "$name does not match the manifest sha256." }
        Move-Item -Force $file (Join-Path $dir "$name.exe")
        Write-Host "installed $dir\$name.exe"
    }
    # The binaries read this to find the manifest when they update themselves.
    @{ url = $ManifestUrl; pubkey = $PubKey } | ConvertTo-Json -Compress |
        Set-Content -Encoding ascii (Join-Path $dir 'update.json')
    $path = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($path -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
        Write-Host "added $dir to your PATH; open a new terminal to use it"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
