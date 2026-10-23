# Releases para Windows

A versão do aplicativo precisa ser igual em `apps/desktop/src-tauri/Cargo.toml` e no campo `version` de `apps/desktop/src-tauri/tauri.conf.json`. Use SemVer estável sem `v` nos arquivos; por exemplo, a tag `v1.0.1` exige `1.0.1` nos dois lugares. A versão do pacote npm raiz identifica as ferramentas de build e não controla o aplicativo.

## Configuração única

O repositório público é [nora77zz/VXE-R1-SE-PLUS-NORA](https://github.com/nora77zz/VXE-R1-SE-PLUS-NORA). O segredo `TAURI_SIGNING_PRIVATE_KEY` fica somente nas configurações do GitHub Actions ou no ambiente privado de quem compila. Configure `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` apenas se a chave tiver senha. O campo `plugins.updater.pubkey` em `apps/desktop/src-tauri/tauri.conf.json` recebe somente a chave pública correspondente. Não coloque a chave privada no Git, nos artefatos ou nos logs.

## Publicar uma versão

1. Atualize a versão SemVer estável em `apps/desktop/src-tauri/Cargo.toml` e `apps/desktop/src-tauri/tauri.conf.json`. Adicione no topo de `docs/changelog.md` uma seção `## [1.0.1] — AAAA-MM-DD` com as notas da versão; essas notas serão incluídas no `latest.json` e na release do GitHub.
2. Regere e valide o lockfile do aplicativo após atualizar sua versão. Esse primeiro `cargo check` é intencionalmente executado sem `--locked`, para atualizar a entrada local do pacote no `Cargo.lock`; depois, valide o worker:

   ```powershell
   cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml
   cargo check --manifest-path apps/lua-worker/Cargo.toml --locked
   ```

3. Faça commit das fontes, `docs/changelog.md` e do `apps/desktop/src-tauri/Cargo.lock` atualizado. Envie o commit para `main` antes de criar a tag:

   ```powershell
   git add -A
   git commit -m "Prepare release 1.0.1"
   git push origin main
   ```

4. Crie e envie a tag que corresponda exatamente à versão:

   ```powershell
   git tag v1.0.1
   git push origin v1.0.1
   ```

5. A Actions inicia apenas para tags `v*`. No Windows, instala a versão fixada do Tauri CLI, compila primeiro `apps/lua-worker` e então gera o instalador NSIS com artefato de atualização assinado. Builds reproduzíveis usam `--locked` depois que o lockfile atualizado foi commitado.
6. O fluxo prepara exatamente quatro assets: `ATK-HUB_<versão>_x64-setup.exe`, `ATK-HUB_<versão>_x64-setup.exe.sig`, `latest.json` e `checksums.sha256`. O `latest.json` associa a assinatura ao instalador e à URL do asset na tag. A versão embutida na assinatura Minisign é exigida pelo verificador do atualizador.
7. O fluxo cria uma release em rascunho, confere nomes e metadados dos quatro assets e só então publica a release. Uma falha de validação deixa a release como rascunho.

O endpoint usado pelo aplicativo é `https://github.com/nora77zz/VXE-R1-SE-PLUS-NORA/releases/latest/download/latest.json`. A conexão usa HTTPS, e o atualizador verifica a assinatura Minisign com a chave pública compilada no aplicativo. Essa assinatura protege o pacote de atualização; o instalador Windows não tem assinatura Authenticode de certificado comercial. Releases comuns não são disparadas por pushes sem tag.

## Reproduzir o build no Windows

Instale PowerShell 7 (`pwsh`), Node.js 22, a toolchain Rust MSVC e os Build Tools do Visual Studio. PowerShell 7 é usado somente pelas ferramentas de build/release; o aplicativo instalado não depende dele. Depois, na raiz do checkout:

```powershell
npm ci
$env:TAURI_SIGNING_PRIVATE_KEY = '<chave obtida por canal privado>'
npm run release:build
```

`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` é opcional para uma chave sem senha. O comando cria o instalador e o `.sig` usando o Tauri CLI fixado no `package-lock.json`; ele não publica uma release.

Para validar e preparar manualmente os assets sem publicar:

```powershell
./scripts/release/prepare-release.ps1 `
  -Version 1.0.0 `
  -Tag v1.0.0 `
  -Repository nora77zz/VXE-R1-SE-PLUS-NORA
```

O diretório padrão é `release-artifacts/v<versão>`, ignorado pelo Git. O preparador rejeita tag e versão divergentes, divergência entre Cargo e Tauri, instaladores ambíguos e assinatura ausente. `publish-release.ps1` é usado pelo workflow; requer `gh` autenticado com permissão `contents:write`. O histórico e as notas versionadas ficam em [changelog.md](changelog.md).
