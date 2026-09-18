$ErrorActionPreference = 'Stop'
$repo = 'zeebo0-UI/Fetchman'
$release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ 'User-Agent' = 'Fetchman-installer' }
$asset = $release.assets | Where-Object { $_.name -match 'windows-x86_64\.zip$' } | Select-Object -First 1
if (-not $asset) { throw 'No Windows x64 Fetchman release is available.' }
$checksum = $release.assets | Where-Object { $_.name -eq ($asset.name + '.sha256') } | Select-Object -First 1
$temp = Join-Path ([IO.Path]::GetTempPath()) ('fetchman-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null
try {
  $zip = Join-Path $temp $asset.name
  Invoke-WebRequest $asset.browser_download_url -OutFile $zip
  if ($checksum) { $sum = (Invoke-WebRequest $checksum.browser_download_url).Content.Trim().Split()[0].ToLower(); if ((Get-FileHash $zip -Algorithm SHA256).Hash.ToLower() -ne $sum) { throw 'Checksum verification failed.' } }
  $install = Join-Path $env:LOCALAPPDATA 'Fetchman\bin'
  New-Item -ItemType Directory -Force -Path $install | Out-Null
  Expand-Archive $zip -DestinationPath $temp\unpacked -Force
  Copy-Item (Get-ChildItem $temp\unpacked -Filter fetchman.exe -Recurse | Select-Object -First 1).FullName (Join-Path $install 'fetchman.exe') -Force
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';') -notcontains $install) { [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';') + ';' + $install).Trim(';')), 'User') }
  Write-Host "Fetchman installed to $install"; Write-Host 'Open a new terminal, then run: fetchman --help'
} finally { Remove-Item $temp -Recurse -Force -ErrorAction SilentlyContinue }
