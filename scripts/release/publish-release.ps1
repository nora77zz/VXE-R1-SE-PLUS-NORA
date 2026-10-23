param(
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][string]$ArtifactsDirectory,
    [string]$Repository = 'nora77zz/VXE-R1-SE-PLUS-NORA'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-common.ps1')
Assert-StableSemVer -Version $Version
if ($Tag -cne "v$Version") {
    throw "Tag '$Tag' não corresponde à versão '$Version'; use v$Version."
}
if ($Repository -cnotmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw "GitHub repository '$Repository' deve estar no formato owner/repository."
}
if (-not $env:GH_TOKEN -and -not $env:GITHUB_TOKEN) {
    throw 'Defina GH_TOKEN ou GITHUB_TOKEN com permissão contents:write.'
}
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw 'GitHub CLI (gh) não está disponível.'
}

$artifactsPath = (Resolve-Path -LiteralPath $ArtifactsDirectory).Path
$installerName = "ATK-HUB_${Version}_x64-setup.exe"
$expectedNames = @(
    $installerName,
    "$installerName.sig",
    'latest.json',
    'checksums.sha256'
)
$actualFiles = @(Get-ChildItem -LiteralPath $artifactsPath -File)
$actualNames = @($actualFiles | ForEach-Object Name | Sort-Object)
$expectedSorted = @($expectedNames | Sort-Object)
if (($actualNames -join "`n") -cne ($expectedSorted -join "`n")) {
    throw "Pasta de release deve conter exatamente: $($expectedSorted -join ', '). Encontrado: $($actualNames -join ', ')."
}

$latest = Get-Content -LiteralPath (Join-Path $artifactsPath 'latest.json') -Raw | ConvertFrom-Json
if ([string]$latest.version -cne $Version) {
    throw "latest.json contém versão inesperada: $($latest.version)."
}
$expectedUrl = "https://github.com/$Repository/releases/download/$Tag/$installerName"
if ([string]$latest.platforms.'windows-x86_64'.url -cne $expectedUrl) {
    throw 'URL do atualizador em latest.json não aponta para o pacote desta tag e repositório.'
}
$signaturePath = Join-Path $artifactsPath "$installerName.sig"
$signature = [System.IO.File]::ReadAllText($signaturePath)
Assert-ReleaseSignature -Signature $signature -Version $Version
if ([string]$latest.platforms.'windows-x86_64'.signature -cne $signature) {
    throw 'Assinatura em latest.json não corresponde ao conteúdo do asset .sig.'
}

$checksumPath = Join-Path $artifactsPath 'checksums.sha256'
$checksumLines = [System.IO.File]::ReadAllLines($checksumPath)
$checksumNames = @($installerName, "$installerName.sig")
if ($checksumLines.Count -ne $checksumNames.Count) {
    throw 'checksums.sha256 não contém exatamente o instalador e seu asset .sig.'
}
for ($index = 0; $index -lt $checksumNames.Count; $index++) {
    $match = [regex]::Match($checksumLines[$index], '^(?<digest>[0-9a-f]{64})  (?<name>[^/\\]+)$')
    if (-not $match.Success -or $match.Groups['name'].Value -cne $checksumNames[$index]) {
        throw 'checksums.sha256 contém uma linha inválida ou um asset inesperado.'
    }
    $actualDigest = (Get-FileHash -LiteralPath (Join-Path $artifactsPath $checksumNames[$index]) -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($match.Groups['digest'].Value -cne $actualDigest) {
        throw "SHA-256 em checksums.sha256 não confere para $($checksumNames[$index])."
    }
}

$notesPath = Join-Path ([System.IO.Path]::GetTempPath()) "atk-hub-release-$Version.md"
$notes = Get-ReleaseNotes -Version $Version
Write-Utf8NoBom -Path $notesPath -Content "$notes`n"
try {
    $assetPaths = @($expectedNames | ForEach-Object { Join-Path $artifactsPath $_ })
    Write-Output "Criando release draft $Tag e enviando os quatro artefatos validados..."
    & gh release create $Tag @assetPaths --repo $Repository --draft --verify-tag --title "ATK HUB $Version" --notes-file $notesPath
    if ($LASTEXITCODE -ne 0) {
        throw "gh release create falhou (código $LASTEXITCODE); a release não foi publicada."
    }

    $viewRaw = & gh release view $Tag --repo $Repository --json isDraft,assets
    if ($LASTEXITCODE -ne 0) {
        throw "Não foi possível verificar os assets do draft $Tag; ele permanece em draft."
    }
    $view = ($viewRaw -join "`n") | ConvertFrom-Json
    if (-not $view.isDraft) {
        throw 'GitHub retornou uma release não draft antes da verificação final.'
    }
    $uploadedNames = @($view.assets | ForEach-Object name | Sort-Object)
    if (($uploadedNames -join "`n") -cne ($expectedSorted -join "`n")) {
        throw "Draft não contém exatamente os assets esperados; ele permanece não publicado."
    }

    & gh release edit $Tag --repo $Repository --draft=false
    if ($LASTEXITCODE -ne 0) {
        throw "Não foi possível publicar o draft $Tag; confira o estado no GitHub."
    }
}
finally {
    Remove-Item -LiteralPath $notesPath -Force -ErrorAction SilentlyContinue
}

Write-Output "Release $Tag publicada em https://github.com/$Repository/releases/tag/$Tag"
