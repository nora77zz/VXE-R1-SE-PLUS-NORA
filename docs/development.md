# Desenvolvimento

O repositório organiza o aplicativo em `apps/desktop/src-tauri`, a interface embarcada em `apps/desktop/web` e o worker Lua em `apps/lua-worker`. As bibliotecas compartilhadas ficam em `crates/`; os utilitários Rust de diagnóstico mantidos ficam em `tools/diagnostics/`.

## Pré-requisitos no Windows

- Windows 10 ou 11, x64.
- Rust 1.93.0 com alvo MSVC e o Windows SDK (`rust-toolchain.toml` fixa a versão).
- Node.js 22 e npm.
- PowerShell 7 (`pwsh`) para os scripts locais de build e release; o aplicativo instalado não depende do PowerShell.
- Build Tools do Visual Studio com compilador C++.
- WebView2 Runtime para executar o aplicativo; o instalador pode baixá-lo se necessário.

## Build local

Na raiz do repositório, instale a versão fixada do Tauri CLI e compile o worker antes do desktop:

```powershell
npm ci
cargo build --manifest-path apps/lua-worker/Cargo.toml --release --locked
cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml --locked
```

O build de distribuição que produz instalador e assinatura requer a chave privada de atualização em `TAURI_SIGNING_PRIVATE_KEY` no ambiente. Execute `npm run release:build` para esse build. Consulte [releases.md](releases.md) para a publicação pelo GitHub Actions.

## Testes direcionados

Execute apenas os testes novos ou modificados e os que dependem diretamente deles. Se um caso falhar, repita somente o caso com falha e seus dependentes necessários. Os testes de interface ficam em `tests/frontend/`; os testes do worker ficam em `apps/lua-worker/tests/`. Os testes de layout usam Playwright 1.62.1, instalado por `npm ci`. Para instalar o Chromium do Playwright uma vez, execute `npx playwright install chromium`; também é possível apontar `BROWSER_EXECUTABLE` para um Chrome já instalado.

## Dependências e dados locais

`apps/desktop/src-tauri/Cargo.lock`, `apps/lua-worker/Cargo.lock` e `package-lock.json` devem acompanhar o código necessário para builds reproduzíveis. Artefatos de build, capturas locais, evidências e chaves privadas não pertencem ao repositório. Os utilitários de diagnóstico mantidos estão em `tools/diagnostics/hid-inspector` e `tools/diagnostics/device-info`; outras ferramentas de investigação locais são ignoradas pelo Git.
