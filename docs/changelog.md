# Histórico de versões

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
