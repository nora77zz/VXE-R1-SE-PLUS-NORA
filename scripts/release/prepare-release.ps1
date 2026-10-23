param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Tag,
    [string]$Repository = 'nora77zz/VXE-R1-SE-PLUS-NORA',
    [string]$BundleDirectory,
    [string]$OutputDirectory,
    [string]$CargoManifestPath,
    [string]$TauriConfigPath
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-common.ps1')

$repoRoot = Get-ReleaseRepositoryRoot
if (-not $CargoManifestPath) {
    $CargoManifestPath = Join-Path $repoRoot 'apps/desktop/src-tauri/Cargo.toml'
}
if (-not $TauriConfigPath) {
    $TauriConfigPath = Join-Path $repoRoot 'apps/desktop/src-tauri/tauri.conf.json'
}
if (-not $BundleDirectory) {
    $BundleDirectory = Join-Path $repoRoot 'apps/desktop/src-tauri/target/release/bundle/nsis'
}
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repoRoot "release-artifacts/v$Version"
}

Assert-ReleaseVersion -Version $Version -Tag $Tag -CargoManifestPath $CargoManifestPath -TauriConfigPath $TauriConfigPath
if ($Repository -cnotmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "GitHub repository '$Repository' deve estar no formato owner/repository."
}
if (-not (Test-Path -LiteralPath $BundleDirectory -PathType Container)) {
    throw "Pasta NSIS não encontrada: $BundleDirectory"
}

$installers = @(Get-ChildItem -LiteralPath $BundleDirectory -File -Filter '*-setup.exe')
if ($installers.Count -ne 1) {
    throw "Esperava exatamente um instalador *-setup.exe em $BundleDirectory; encontrei $($installers.Count)."
}

$sourceInstaller = $installers[0]
$sourceSignature = "$($sourceInstaller.FullName).sig"
if (-not (Test-Path -LiteralPath $sourceSignature -PathType Leaf)) {
    throw "Assinatura do instalador NSIS não encontrada: $sourceSignature"
}
$signature = [System.IO.File]::ReadAllText($sourceSignature)
Assert-ReleaseSignature -Signature $signature -Version $Version

$outputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
if (Test-Path -LiteralPath $outputDirectory -PathType Leaf) {
    throw "O caminho de saída existe como arquivo: $outputDirectory"
}
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
$installerName = "ATK-HUB_${Version}_x64-setup.exe"
$signatureName = "$installerName.sig"
$generatedNames = @($installerName, $signatureName, 'latest.json', 'checksums.sha256')
$unexpectedFiles = @(Get-ChildItem -LiteralPath $outputDirectory -File | Where-Object { $_.Name -notin $generatedNames })
if ($unexpectedFiles.Count -gt 0) {
    throw "Pasta de saída contém arquivos alheios ao release: $($unexpectedFiles.Name -join ', ')."
}
foreach ($name in $generatedNames) {
    $existingPath = Join-Path $outputDirectory $name
    if (Test-Path -LiteralPath $existingPath -PathType Leaf) {
        Remove-Item -LiteralPath $existingPath -Force
    }
}
$installerPath = Join-Path $outputDirectory $installerName
$signaturePath = Join-Path $outputDirectory $signatureName
Copy-Item -LiteralPath $sourceInstaller.FullName -Destination $installerPath
Copy-Item -LiteralPath $sourceSignature -Destination $signaturePath

$notes = Get-ReleaseNotes -Version $Version
$assetUrl = "https://github.com/$Repository/releases/download/$Tag/$installerName"
$metadata = [ordered]@{
    version = $Version
    notes = $notes
    pub_date = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture)
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url = $assetUrl
        }
    }
}
$latestPath = Join-Path $outputDirectory 'latest.json'
Write-Utf8NoBom -Path $latestPath -Content (($metadata | ConvertTo-Json -Depth 8) + "`n")

$checksumPaths = @($installerPath, $signaturePath)
$checksumLines = foreach ($path in $checksumPaths) {
    $digest = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    "$digest  $([System.IO.Path]::GetFileName($path))"
}
Write-Utf8NoBom -Path (Join-Path $outputDirectory 'checksums.sha256') -Content (($checksumLines -join "`n") + "`n")

Write-Output "Artefatos preparados em $outputDirectory"
Write-Output "  $installerName"
Write-Output "  $signatureName"
Write-Output '  latest.json'
Write-Output '  checksums.sha256'
