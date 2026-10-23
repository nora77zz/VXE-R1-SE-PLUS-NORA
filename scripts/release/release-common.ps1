Set-StrictMode -Version Latest

function Get-ReleaseRepositoryRoot {
    return [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
}

function Get-CargoPackageVersion {
    param([Parameter(Mandatory)][string]$ManifestPath)

    $manifest = Get-Content -LiteralPath $ManifestPath -Raw
    $packageMatch = [regex]::Match($manifest, '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)')
    if (-not $packageMatch.Success) {
        throw "Não encontrei a seção [package] em $ManifestPath."
    }

    $versionMatch = [regex]::Match($packageMatch.Groups[1].Value, '(?m)^version\s*=\s*"([^"]+)"\s*$')
    if (-not $versionMatch.Success) {
        throw "Não encontrei uma versão literal em $ManifestPath."
    }

    return $versionMatch.Groups[1].Value
}

function Assert-StableSemVer {
    param([Parameter(Mandatory)][string]$Version)

    if ($Version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') {
        throw "Versão '$Version' inválida. Releases estáveis devem usar SemVer MAJOR.MINOR.PATCH sem prefixo v."
    }
}

function Assert-ReleaseSignature {
    param(
        [Parameter(Mandatory)][string]$Signature,
        [Parameter(Mandatory)][string]$Version
    )

    if ([string]::IsNullOrWhiteSpace($Signature)) {
        throw 'A assinatura do instalador NSIS está vazia.'
    }

    $externalSignature = $Signature.Trim()
    if ($externalSignature -cnotmatch '^[A-Za-z0-9+/]+={0,2}$') {
        throw 'A assinatura externa do instalador contém Base64 inválido.'
    }
    try {
        $signatureBytes = [Convert]::FromBase64String($externalSignature)
        $signatureText = [System.Text.UTF8Encoding]::new($false, $true).GetString($signatureBytes)
    }
    catch {
        throw 'A assinatura externa do instalador contém Base64 inválido ou não é texto UTF-8.'
    }

    $lines = @([regex]::Split($signatureText, "`r?`n"))
    if ($lines.Count -gt 0 -and $lines[-1] -ceq '') {
        $lines = @($lines[0..($lines.Count - 2)])
    }
    if ($lines.Count -ne 4 -or $lines[0] -cnotmatch '^untrusted comment: .+$' -or $lines[2] -cnotmatch '^trusted comment: .+$') {
        throw 'Assinatura do instalador não tem o formato Minisign esperado.'
    }

    foreach ($lineIndex in @(1, 3)) {
        $encoded = [string]$lines[$lineIndex]
        if ($encoded -cnotmatch '^[A-Za-z0-9+/]+={0,2}$') {
            throw 'A assinatura do instalador contém um campo Base64 inválido.'
        }
        try {
            [void][Convert]::FromBase64String($encoded)
        }
        catch {
            throw 'A assinatura do instalador contém um campo Base64 inválido.'
        }
    }

    $trustedComment = $lines[2].Substring('trusted comment: '.Length)
    $commentFields = @($trustedComment.Split([char]9))
    $versionFields = @($commentFields | Where-Object { $_.StartsWith('version:', [System.StringComparison]::Ordinal) })
    if ($versionFields.Count -ne 1) {
        throw 'O trusted comment da assinatura deve conter exatamente um campo version:<SemVer> separado por TAB.'
    }

    $signedVersion = $versionFields[0].Substring('version:'.Length)
    Assert-StableSemVer -Version $signedVersion
    if ($signedVersion -cne $Version) {
        throw "Versão assinada no trusted comment ($signedVersion) difere da versão do release ($Version)."
    }
}

function Assert-ReleaseVersion {
    param(
        [Parameter(Mandatory)][string]$Version,
        [Parameter(Mandatory)][string]$Tag,
        [Parameter(Mandatory)][string]$CargoManifestPath,
        [Parameter(Mandatory)][string]$TauriConfigPath
    )

    Assert-StableSemVer -Version $Version
    if ($Tag -cne "v$Version") {
        throw "Tag '$Tag' não corresponde à versão '$Version'; use v$Version."
    }

    $cargoVersion = Get-CargoPackageVersion -ManifestPath $CargoManifestPath
    $tauriConfig = Get-Content -LiteralPath $TauriConfigPath -Raw | ConvertFrom-Json
    if ($cargoVersion -cne $Version) {
        throw "Versão informada ($Version) difere de apps/desktop/src-tauri/Cargo.toml ($cargoVersion)."
    }
    if ([string]$tauriConfig.version -cne $Version) {
        throw "Versão informada ($Version) difere de apps/desktop/src-tauri/tauri.conf.json ($($tauriConfig.version))."
    }
    if ($cargoVersion -cne [string]$tauriConfig.version) {
        throw "Versões divergentes entre Cargo.toml ($cargoVersion) e tauri.conf.json ($($tauriConfig.version))."
    }
}

function Get-ReleaseNotes {
    param([Parameter(Mandatory)][string]$Version)

    $changelogPath = Join-Path (Get-ReleaseRepositoryRoot) 'docs/changelog.md'
    if (-not (Test-Path -LiteralPath $changelogPath -PathType Leaf)) {
        throw "docs/changelog.md não encontrado: $changelogPath"
    }

    $changelog = Get-Content -LiteralPath $changelogPath -Raw
    $versionPattern = [regex]::Escape($Version)
    $pattern = "(?ms)^## \[$versionPattern\][^\r\n]*\r?\n(?<notes>.*?)(?=^## \[|\z)"
    $match = [regex]::Match($changelog, $pattern)
    if (-not $match.Success) {
        throw "docs/changelog.md não contém uma seção para a versão $Version."
    }

    $notes = $match.Groups['notes'].Value.Trim()
    if ([string]::IsNullOrWhiteSpace($notes)) {
        throw "A seção $Version em docs/changelog.md não contém notas de release."
    }

    return $notes
}

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Content
    )

    $encoding = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $Content, $encoding)
}
