param([string]$Case)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot '..\release-common.ps1')

$releaseTools = Split-Path -Parent $PSScriptRoot
$prepareScript = Join-Path $releaseTools 'prepare-release.ps1'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) "atk-release-tests-$([Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $tempRoot | Out-Null

function Assert-Condition {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Invoke-ExpectedFailure {
    param([scriptblock]$Action, [string]$MessagePart)
    try {
        & $Action
    }
    catch {
        if ($_.Exception.Message -like "*$MessagePart*") { return }
        throw
    }
    throw "Esperava falha contendo '$MessagePart'."
}

function New-TestSignature {
    param(
        [Parameter(Mandatory)][string]$SignedVersion,
        [switch]$InvalidInnerBase64
    )
    $signatureLine = if ($InvalidInnerBase64) { 'not-base64!' } else { 'AQIDBA==' }
    $minisignText = "untrusted comment: test signature`n$signatureLine`ntrusted comment: timestamp:123`tversion:$SignedVersion`nBAUGBw==`n"
    return [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($minisignText))
}

function New-PublishFixture {
    param([Parameter(Mandatory)][string]$Directory, [Parameter(Mandatory)][string]$SignatureText)

    New-Item -ItemType Directory -Path $Directory | Out-Null
    $installerName = 'ATK-HUB_1.0.0_x64-setup.exe'
    $installerPath = Join-Path $Directory $installerName
    $signaturePath = Join-Path $Directory "$installerName.sig"
    [System.IO.File]::WriteAllBytes($installerPath, [byte[]](0x4d, 0x5a, 0x10, 0x20))
    Write-Utf8NoBom -Path $signaturePath -Content $SignatureText
    $metadata = [ordered]@{
        version = '1.0.0'
        notes = 'test notes'
        pub_date = '2026-10-23T00:00:00Z'
        platforms = [ordered]@{
            'windows-x86_64' = [ordered]@{
                signature = $SignatureText
                url = 'https://github.com/nora77zz/VXE-R1-SE-PLUS-NORA/releases/download/v1.0.0/ATK-HUB_1.0.0_x64-setup.exe'
            }
        }
    }
    Write-Utf8NoBom -Path (Join-Path $Directory 'latest.json') -Content (($metadata | ConvertTo-Json -Depth 8) + "`n")
    $checksumLines = foreach ($path in @($installerPath, $signaturePath)) {
        $digest = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        "$digest  $([System.IO.Path]::GetFileName($path))"
    }
    Write-Utf8NoBom -Path (Join-Path $Directory 'checksums.sha256') -Content (($checksumLines -join "`n") + "`n")
    return $Directory
}

function Invoke-WithGhStub {
    param([Parameter(Mandatory)][scriptblock]$Action)

    $stubDirectory = Join-Path $tempRoot 'gh-stub'
    New-Item -ItemType Directory -Force -Path $stubDirectory | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $stubDirectory 'gh.cmd'), "@echo off`r`nexit /b 1`r`n", [System.Text.Encoding]::ASCII)
    $previousPath = $env:PATH
    $previousGhToken = $env:GH_TOKEN
    try {
        $env:PATH = "$stubDirectory;$previousPath"
        $env:GH_TOKEN = 'test-token-only'
        & $Action
    }
    finally {
        $env:PATH = $previousPath
        if ($null -eq $previousGhToken) {
            Remove-Item Env:GH_TOKEN -ErrorAction SilentlyContinue
        }
        else {
            $env:GH_TOKEN = $previousGhToken
        }
    }
}

try {
    $cargoPath = Join-Path $tempRoot 'Cargo.toml'
    $configPath = Join-Path $tempRoot 'tauri.conf.json'
    $bundlePath = Join-Path $tempRoot 'nsis'
    $outputPath = Join-Path $tempRoot 'output'
    New-Item -ItemType Directory -Path $bundlePath | Out-Null
    Write-Utf8NoBom -Path $cargoPath -Content ((@('[package]', 'name = "atk-hub"', 'version = "1.0.0"', '') -join "`n"))
    Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.0"}'
    [System.IO.File]::WriteAllBytes((Join-Path $bundlePath 'source_1.0.0_x64-setup.exe'), [byte[]](0x4d, 0x5a, 0x01))
    $signatureText = New-TestSignature -SignedVersion '1.0.0'
    Write-Utf8NoBom -Path (Join-Path $bundlePath 'source_1.0.0_x64-setup.exe.sig') -Content $signatureText

    if (-not $Case -or $Case -eq 'metadata_success_has_exact_signed_windows_release_assets') {
        & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -Repository 'nora77zz/VXE-R1-SE-PLUS-NORA' -BundleDirectory $bundlePath -OutputDirectory $outputPath -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        $metadataRaw = Get-Content -LiteralPath (Join-Path $outputPath 'latest.json') -Raw
        $metadata = $metadataRaw | ConvertFrom-Json
        Assert-Condition ($metadata.version -ceq '1.0.0') 'latest.json não preservou a versão.'
        Assert-Condition ($metadata.platforms.'windows-x86_64'.signature -ceq $signatureText) 'latest.json não preserva o conteúdo bruto do .exe.sig.'
        Assert-Condition ($metadata.platforms.'windows-x86_64'.url -ceq 'https://github.com/nora77zz/VXE-R1-SE-PLUS-NORA/releases/download/v1.0.0/ATK-HUB_1.0.0_x64-setup.exe') 'latest.json não usa a URL do instalador da tag.'
        Assert-Condition ([regex]::IsMatch($metadataRaw, '"pub_date"\s*:\s*"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z"')) 'pub_date não está como timestamp ISO UTC terminado em Z.'
        Assert-Condition ((Get-ChildItem -LiteralPath $outputPath -File).Count -eq 4) 'A pasta deve ter exatamente quatro assets de release.'
        Assert-Condition ((Get-Content -LiteralPath (Join-Path $outputPath 'checksums.sha256')).Count -eq 2) 'checksums.sha256 deve conter hash do instalador e da assinatura.'
        $stagedSignature = [System.IO.File]::ReadAllText((Join-Path $outputPath 'ATK-HUB_1.0.0_x64-setup.exe.sig'))
        Assert-Condition ($stagedSignature -ceq $signatureText) 'O arquivo de assinatura publicado deve manter os bytes originais.'
        Write-Output 'PASS metadata_success_has_exact_signed_windows_release_assets'
    }

    if (-not $Case -or $Case -eq 'release_notes_are_read_from_the_requested_changelog_section') {
        $releaseNotes = Get-ReleaseNotes -Version '1.0.0'
        Assert-Condition ($releaseNotes.Contains('Instalador para Windows x64')) 'As notas não vieram da seção 1.0.0 de docs/changelog.md.'
        Assert-Condition ($releaseNotes.Contains('desinstalação pelo Windows')) 'As notas não preservaram os caracteres UTF-8 do changelog.'
        Assert-Condition (-not $releaseNotes.Contains('## [1.0.0]')) 'O cabeçalho da seção não deve ser publicado como nota.'
        Write-Output 'PASS release_notes_are_read_from_the_requested_changelog_section'
    }

    if (-not $Case -or $Case -eq 'tag_version_mismatch_is_rejected_before_staging') {
        $tagMismatchOutput = Join-Path $tempRoot 'tag-mismatch'
        Invoke-ExpectedFailure -MessagePart "Tag 'v1.0.1'" -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.1' -BundleDirectory $bundlePath -OutputDirectory $tagMismatchOutput -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Assert-Condition (-not (Test-Path -LiteralPath $tagMismatchOutput)) 'Tag mismatch criou a pasta de artefatos.'
        Write-Output 'PASS tag_version_mismatch_is_rejected_before_staging'
    }

    if (-not $Case -or $Case -eq 'cargo_tauri_manifest_version_mismatch_is_rejected') {
        Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.1"}'
        $manifestMismatchOutput = Join-Path $tempRoot 'manifest-mismatch'
        Invoke-ExpectedFailure -MessagePart 'difere de apps/desktop/src-tauri/tauri.conf.json' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory $manifestMismatchOutput -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Assert-Condition (-not (Test-Path -LiteralPath $manifestMismatchOutput)) 'Divergência entre Cargo e config criou a pasta de artefatos.'
        Write-Output 'PASS cargo_tauri_manifest_version_mismatch_is_rejected'
    }

    if (-not $Case -or $Case -eq 'missing_executable_signature_is_rejected') {
        Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.0"}'
        $signaturePath = Join-Path $bundlePath 'source_1.0.0_x64-setup.exe.sig'
        Remove-Item -LiteralPath $signaturePath -Force
        Invoke-ExpectedFailure -MessagePart 'Assinatura do instalador NSIS não encontrada' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory (Join-Path $tempRoot 'missing-signature') -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Write-Output 'PASS missing_executable_signature_is_rejected'
    }

    if (-not $Case -or $Case -eq 'signed_version_mismatch_is_rejected_before_staging') {
        Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.0"}'
        $signaturePath = Join-Path $bundlePath 'source_1.0.0_x64-setup.exe.sig'
        Write-Utf8NoBom -Path $signaturePath -Content (New-TestSignature -SignedVersion '1.0.1')
        $signedVersionOutput = Join-Path $tempRoot 'signed-version-mismatch'
        Invoke-ExpectedFailure -MessagePart 'Versão assinada no trusted comment (1.0.1)' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory $signedVersionOutput -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Assert-Condition (-not (Test-Path -LiteralPath $signedVersionOutput)) 'Versão assinada divergente criou a pasta de artefatos.'
        Write-Output 'PASS signed_version_mismatch_is_rejected_before_staging'
    }

    if (-not $Case -or $Case -eq 'invalid_signature_base64_is_rejected_before_staging') {
        Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.0"}'
        $signaturePath = Join-Path $bundlePath 'source_1.0.0_x64-setup.exe.sig'
        $invalidSignature = New-TestSignature -SignedVersion '1.0.0' -InvalidInnerBase64
        Write-Utf8NoBom -Path $signaturePath -Content $invalidSignature
        Invoke-ExpectedFailure -MessagePart 'campo Base64 inválido' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory (Join-Path $tempRoot 'invalid-signature') -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Write-Output 'PASS invalid_signature_base64_is_rejected_before_staging'
    }

    if (-not $Case -or $Case -eq 'invalid_outer_signature_base64_is_rejected_before_staging') {
        Write-Utf8NoBom -Path $configPath -Content '{"version":"1.0.0"}'
        $signaturePath = Join-Path $bundlePath 'source_1.0.0_x64-setup.exe.sig'
        Write-Utf8NoBom -Path $signaturePath -Content 'not-base64!'
        Invoke-ExpectedFailure -MessagePart 'assinatura externa do instalador contém Base64 inválido' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory (Join-Path $tempRoot 'invalid-outer-signature') -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Write-Output 'PASS invalid_outer_signature_base64_is_rejected_before_staging'
    }

    if (-not $Case -or $Case -eq 'ambiguous_setup_installers_are_rejected') {
        [System.IO.File]::WriteAllBytes((Join-Path $bundlePath 'another_1.0.0_x64-setup.exe'), [byte[]](0x4d, 0x5a, 0x02))
        Invoke-ExpectedFailure -MessagePart 'encontrei 2' -Action {
            & $prepareScript -Version '1.0.0' -Tag 'v1.0.0' -BundleDirectory $bundlePath -OutputDirectory (Join-Path $tempRoot 'ambiguous-installer') -CargoManifestPath $cargoPath -TauriConfigPath $configPath
        }
        Write-Output 'PASS ambiguous_setup_installers_are_rejected'
    }

    if (-not $Case -or $Case -eq 'metadata_signature_mismatch_is_rejected_before_gh') {
        $publishPath = Join-Path $releaseTools 'publish-release.ps1'
        $publishDirectory = New-PublishFixture -Directory (Join-Path $tempRoot 'publish-signature-mismatch') -SignatureText (New-TestSignature -SignedVersion '1.0.0')
        $latestPath = Join-Path $publishDirectory 'latest.json'
        $latest = Get-Content -LiteralPath $latestPath -Raw | ConvertFrom-Json
        $latest.platforms.'windows-x86_64'.signature = New-TestSignature -SignedVersion '1.0.1'
        Write-Utf8NoBom -Path $latestPath -Content (($latest | ConvertTo-Json -Depth 8) + "`n")
        Invoke-WithGhStub {
            Invoke-ExpectedFailure -MessagePart 'Assinatura em latest.json não corresponde' -Action {
                & $publishPath -Version '1.0.0' -Tag 'v1.0.0' -ArtifactsDirectory $publishDirectory
            }
        }
        Write-Output 'PASS metadata_signature_mismatch_is_rejected_before_gh'
    }

    if (-not $Case -or $Case -eq 'tampered_asset_checksum_is_rejected_before_gh') {
        $publishPath = Join-Path $releaseTools 'publish-release.ps1'
        $publishDirectory = New-PublishFixture -Directory (Join-Path $tempRoot 'publish-checksum-tamper') -SignatureText (New-TestSignature -SignedVersion '1.0.0')
        [System.IO.File]::WriteAllBytes((Join-Path $publishDirectory 'ATK-HUB_1.0.0_x64-setup.exe'), [byte[]](0x4d, 0x5a, 0x99))
        Invoke-WithGhStub {
            Invoke-ExpectedFailure -MessagePart 'SHA-256 em checksums.sha256 não confere para ATK-HUB_1.0.0_x64-setup.exe' -Action {
                & $publishPath -Version '1.0.0' -Tag 'v1.0.0' -ArtifactsDirectory $publishDirectory
            }
        }
        Write-Output 'PASS tampered_asset_checksum_is_rejected_before_gh'
    }
}
finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
