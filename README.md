# ATK HUB

ATK HUB é um aplicativo Windows para o VXE R1 SE+ Nora. Ele reúne as funções do mouse, diagnóstico do receptor e execução de scripts Lua em um único aplicativo desktop.

## Instalar

Baixe o instalador mais recente em [Releases](https://github.com/nora77zz/VXE-R1-SE-PLUS-NORA/releases/latest) e execute `ATK-HUB_<versão>_x64-setup.exe`. A instalação é feita para o usuário atual e inclui a opção padrão de desinstalação do Windows. Se o WebView2 Runtime estiver ausente, o instalador obtém o runtime necessário.

## Atualizações

O aplicativo consulta a release pública do GitHub por HTTPS. Quando há uma versão nova, pergunta se você quer instalar agora. O pacote de atualização é verificado pela chave pública incluída no aplicativo; depois da instalação, o ATK HUB reabre sozinho.

O código do desktop está em `apps/desktop/src-tauri`, a interface em `apps/desktop/web` e o processo Lua em `apps/lua-worker`. Para compilar e publicar uma versão, consulte [docs/development.md](docs/development.md) e [docs/releases.md](docs/releases.md).
