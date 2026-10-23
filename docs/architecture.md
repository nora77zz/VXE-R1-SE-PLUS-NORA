# Arquitetura

## Organização

| Diretório | Responsabilidade |
| --- | --- |
| `apps/desktop/src-tauri` | Aplicativo Windows, janela, bandeja, comandos nativos, diagnóstico e atualizador. |
| `apps/desktop/web` | Interface embarcada, biblioteca de scripts, console, diagnóstico e confirmação de atualização. |
| `apps/lua-worker` | Processo separado que executa os scripts Lua. |
| `crates` | Bibliotecas de protocolo, comunicação com o dispositivo, configuração, Lua e entrada do Windows. |
| `scripts/release` | Compilação do instalador, preparação do manifesto e publicação dos artefatos. |
| `tests/frontend` | Testes das extensões da interface. |
| `tools/diagnostics` | Utilitários de inspeção HID e leitura de informações do dispositivo. |
| `docs` | Guias de desenvolvimento, API Lua, arquitetura e releases. |

Arquivos de investigação anteriores, capturas e diretórios de compilação ficam fora do conteúdo versionado. A interface alternativa antiga não faz parte do aplicativo distribuído.

## Aplicativo e execução Lua

O Tauri embarca os arquivos web indicados por `frontendDist`. As extensões da interface invocam comandos nativos; a comunicação USB passa pelo ator serializado de `device_bridge.rs`.

O processo Lua é distribuído como recurso do aplicativo. O backend procura primeiro esse recurso, de modo que a instalação não depende do diretório de desenvolvimento. A execução isolada e o protocolo entre os processos ficam nas bibliotecas `ghub-lua` e `lua-process`.

A biblioteca de scripts salva os textos e o histórico localmente. O diagnóstico diferencia a enumeração do receptor da resposta à consulta de identidade. Esses estados não são uma medição de latência ou uma validação completa do hardware.

## Atualizações

`app_updates.js` consulta os comandos `app_update_status` e `app_update_check`. Uma oferta abre o diálogo com as versões e as notas. O botão de instalação envia apenas a versão confirmada a `app_update_install`; a interface não fornece um caminho de executável ou uma URL de download.

`app_updates.rs` mantém a oferta obtida pelo plugin oficial e impede operações simultâneas. O pacote é baixado e verificado antes de parar o worker Lua. A seção de parada e instalação compartilha uma trava com o início de scripts, para impedir uma nova execução durante a troca dos arquivos. Erros preservam uma oferta válida para nova tentativa.

No Windows, o plugin chama o instalador com a opção de relançamento. A bandeja oferece uma verificação manual, além da consulta inicial e da consulta periódica da interface. Uma release deve ser publicada somente depois que todos os seus arquivos estiverem disponíveis.

## Referências de organização

A separação entre a interface web e `src-tauri` segue a [estrutura de projeto do Tauri](https://v2.tauri.app/start/project-structure/). O papel do README e dos guias de contribuição segue as [orientações do GitHub](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-readmes). As relações entre os pacotes Rust seguem a [documentação de workspaces do Cargo](https://doc.rust-lang.org/cargo/reference/workspaces.html).
