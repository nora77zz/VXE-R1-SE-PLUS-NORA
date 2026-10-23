# Isolamento do runtime Lua

`lua-process` executa os scripts em um processo separado pelo executável que o aplicativo informa explicitamente. O pai exige caminho absoluto para um arquivo existente, rejeita UNC no Windows e usa `Command::new` sem procurar no `PATH` e sem shell. `lua-worker` recebe a fonte Lua e a configuração em JSON Lines pelo stdin; stdout é reservado ao protocolo. Os logs de compilação do próprio processo vão para stderr, cuja cauda é limitada.

O worker implementa a interface pública atual de `ghub-lua` por uma ponte IPC. Cada chamada de `InputBackend` vira uma mensagem com identificador e espera uma resposta por prazo finito. A thread de leitura do filho continua roteando respostas enquanto o runtime processa um callback Lua. O pai valida cada operação e executa o `InputBackend` configurado pelo aplicativo. Este código não fornece implementação de `SendInput` nem injeta entrada real.

O pai registra um botão somente depois que `press_mouse_button` retorna sucesso. Durante o encerramento, bloqueia novas operações, envia `stop`, atende pedidos de soltura do worker durante a janela cooperativa e encerra o processo quando o prazo acaba. Então tenta soltar todos os botões ainda registrados no pai e inclui falhas de soltura no relatório. Essa etapa cobre Lua preso dentro de uma chamada C não yieldable, como um callback de `string.gsub`, porque o sistema operacional encerra o processo filho. Um backend do aplicativo ainda deve cumprir o contrato `InputBackend` de responder prontamente; chamadas de backend travadas não podem ser interrompidas por esta camada.

Os limites atuais são: fonte de 1 MiB, frame JSON de fonte mais 64 KiB, filas de eventos do pai e do filho limitadas a 128, até 32 respostas IPC pendentes, nomes de evento de 256 bytes, família de 128 bytes, entrada de log de 8 KiB, texto de erro de 1 KiB, até 32 mensagens no relatório e cauda de stderr de 8 KiB. O `stop` tem prazo próprio além da janela cooperativa; se o broker não concluir a limpeza nesse prazo, a API retorna erro de timeout. Frames JSON malformados, incompletos ou acima do limite encerram o canal; nenhum conteúdo de um frame inválido chega ao backend.

Na integração, o aplicativo cria `LuaProcessRuntime` com seu backend, passa o caminho absoluto empacotado do `lua-worker` e a fonte selecionada a `start`, envia eventos com `dispatch` e encerra a sessão com `stop`. O crate oferece o limite do processo, IPC e broker; ele não integra interface gráfica, permissões de entrada, persistência de scripts nem compilação/empacotamento dos recursos do Tauri.

## Verificação direcionada

O worker e o pai são workspaces Cargo independentes para não alterar os manifests nem o lockfile da raiz. Compile-os com:

```powershell
cargo check --manifest-path crates/lua-process/Cargo.toml
cargo check --manifest-path apps/lua-worker/Cargo.toml --bin lua-worker
```

Os testes de processo ficam junto ao pacote do worker porque o Cargo disponibiliza os executáveis testados ao alvo de integração. Eles usam apenas um `InputBackend` em memória e fixtures locais:

```powershell
cargo test --manifest-path apps/lua-worker/Cargo.toml --test process
```

Eles verificam cancelamento de loop protegido, encerramento forçado de callback preso em `string.gsub`, limpeza depois de erro Lua e EOF, e ausência de chamada ao backend quando o filho envia framing inválido. Nenhum teste toca no mouse ou firmware.
