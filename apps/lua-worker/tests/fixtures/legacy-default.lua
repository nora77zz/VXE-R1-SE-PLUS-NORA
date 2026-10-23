-- Script Lua Host-side para VXE R1 SE+
function OnEvent(event, arg)
    OutputLogMessage("Evento recebido: %s, Arg: %s\n", tostring(event), tostring(arg))
    
    if event == "PROFILE_ACTIVATED" then
        OutputLogMessage(">> [VXE R1 SE+] Perfil ativado com sucesso!\n")
    end

    -- Exemplo: clique com o botão esquerdo (1)
    if event == "MOUSE_BUTTON_PRESSED" and arg == 1 then
        OutputLogMessage(">> Botão 1 (Esquerdo) pressionado\n")
    end
end
