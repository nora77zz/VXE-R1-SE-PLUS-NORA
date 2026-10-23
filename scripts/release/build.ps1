param([string]$Version)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-common.ps1')

$repoRoot = Get-ReleaseRepositoryRoot
$manifestPath = Join-Path $repoRoot 'apps/desktop/src-tauri/Cargo.toml'
$tauriConfigPath = Join-Path $repoRoot 'apps/desktop/src-tauri/tauri.conf.json'
$tauriConfig = Get-Content -LiteralPath $tauriConfigPath -Raw | ConvertFrom-Json
$resolvedVersion = [string]$tauriConfig.version
if ($Version -and $Version -cne $resolvedVersion) {
    throw "Versão solicitada ($Version) difere de tauri.conf.json ($resolvedVersion)."
}

Assert-ReleaseVersion -Version $resolvedVersion -Tag "v$resolvedVersion" -CargoManifestPath $manifestPath -TauriConfigPath $tauriConfigPath
if ([string]$tauriConfig.plugins.updater.pubkey -eq 'PUBLIC_KEY_PLACEHOLDER_REPLACE_BEFORE_RELEASE') {
    throw 'A chave pública do atualizador ainda é placeholder; insira a chave pública antes de compilar.'
}
if ([string]::IsNullOrWhiteSpace($env:TAURI_SIGNING_PRIVATE_KEY)) {
    throw 'Defina TAURI_SIGNING_PRIVATE_KEY no ambiente para gerar os artefatos assinados do atualizador.'
}

$workerManifest = Join-Path $repoRoot 'apps/lua-worker/Cargo.toml'
Write-Output 'Compilando lua-worker para Windows...'
& cargo build --manifest-path $workerManifest --release --locked
if ($LASTEXITCODE -ne 0) {
    throw "A compilação de lua-worker falhou (código $LASTEXITCODE)."
}
$workerPath = Join-Path $repoRoot 'apps/lua-worker/target/release/lua-worker.exe'
if (-not (Test-Path -LiteralPath $workerPath -PathType Leaf)) {
    throw "lua-worker não foi gerado no caminho de recurso esperado: $workerPath"
}

$tauriCli = Join-Path $repoRoot 'node_modules/.bin/tauri.cmd'
if (-not (Test-Path -LiteralPath $tauriCli -PathType Leaf)) {
    throw 'Tauri CLI não encontrado. Execute npm ci na raiz do repositório.'
}

Write-Output 'Gerando o instalador NSIS e o pacote assinado de atualização...'
Push-Location (Join-Path $repoRoot 'apps/desktop/src-tauri')
try {
    & $tauriCli build --bundles nsis -- --locked
    if ($LASTEXITCODE -ne 0) {
        throw "Tauri build falhou (código $LASTEXITCODE)."
    }
}
finally {
    Pop-Location
}

Write-Output "Build concluído para ATK HUB $resolvedVersion."
