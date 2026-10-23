param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Tag,
    [string]$CargoManifestPath,
    [string]$TauriConfigPath
)

. (Join-Path $PSScriptRoot 'release-common.ps1')
$repoRoot = Get-ReleaseRepositoryRoot
if (-not $CargoManifestPath) {
    $CargoManifestPath = Join-Path $repoRoot 'apps/desktop/src-tauri/Cargo.toml'
}
if (-not $TauriConfigPath) {
    $TauriConfigPath = Join-Path $repoRoot 'apps/desktop/src-tauri/tauri.conf.json'
}

Assert-ReleaseVersion -Version $Version -Tag $Tag -CargoManifestPath $CargoManifestPath -TauriConfigPath $TauriConfigPath
Write-Output "Versão validada: $Version ($Tag)."
