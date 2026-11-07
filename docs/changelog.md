# Histórico de versões

## [1.0.3] — 2026-11-07

- O carregamento aparece logo ao conectar o cabo: a barra de diagnóstico mostra "carregando" em menos de 1 segundo e o ícone da bateria muda em seguida.
- A porcentagem da bateria atualiza em tempo real enquanto a janela está aberta. As leituras pausam com a janela escondida ou depois de 1 minuto sem uso do computador, para o mouse continuar podendo dormir.
- A barra de diagnóstico mostra a porcentagem da bateria e indica quando o mouse carrega por um carregador de tomada.
- `atk.time.sleep` nos scripts Lua respeita o tempo pedido: `sleep(16)` passou de cerca de 30 ms para 16 ms, e `sleep(1)` de 15 ms para cerca de 1,6 ms.

## [1.0.2] — 2026-11-07

- O aviso "New Version" do topo agora só aparece quando há uma versão nova do ATK HUB, e "Update now" instala essa versão. Antes ele mostrava a versão 3.2.32 do aplicativo oficial da ATK e não fazia nada ao clicar.
- A página Sobre mostra a versão real do aplicativo.
- Comandos enviados ao mouse respondem bem mais rápido: em sequência, caíram de cerca de 47 ms para 7 ms cada no receptor 2,4 GHz.
- Eventos dos botões do mouse chegam aos scripts Lua sem espera, e o console Lua atualiza a cada 250 ms.
- A barra de diagnóstico fica fixa embaixo também na primeira abertura do aplicativo.

## [1.0.1] — 2026-10-23

- Suporte ao mouse no cabo USB 3554:F58F e alternância automática com o receptor 373B:1085.
- Notificações de conexão do Windows, restauração da interface em cada troca e descarte de respostas do transporte anterior.
- Operações HID fora da thread da janela e interrupção de leituras pendentes quando a conexão muda.
- Diagnóstico com transporte atual e carregamento informado pelo mouse.
- Cache de dispositivos e estado para evitar enumerações repetidas por comando; pausa das consultas Lua quando a janela está oculta.

## [1.0.0] — 2026-10-23

- Instalador para Windows x64 com integração ao menu Iniciar e desinstalação pelo Windows.
- Consulta de atualizações pelo GitHub, confirmação antes da instalação e relançamento do aplicativo.
- Verificação da assinatura do pacote e da versão anunciada.
- Diagnóstico do receptor USB com nova tentativa manual.
- Biblioteca de scripts Lua com nomes, salvamento automático e histórico de versões.
- Atalho global configurável para parar scripts e comando de parada na bandeja.
- Console incremental com filtros, limite de linhas e pausa da rolagem.
- Layout adaptável a janelas menores e escalas de tela diferentes.
