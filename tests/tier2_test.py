#!/usr/bin/env python3
"""Tier 2 — prova o servidor MCP do rustranslator sob um cliente MCP REAL.

Por que nao basta um handshake manual: um script que escreve JSON-RPC a mao nao exercita
keepalive, ciclo de vida, nem a negociacao real de versao. O cliente de verdade e' o que o
Hermes usa por baixo. Aqui: `stdio_client` + `ClientSession` da lib `mcp`.

Uso:
    python3 tests/tier2_test.py

Criterio de sucesso: o `list_tools` devolve a tool `eco`, e a chamada devolve o texto
enviado junto de um instante.
"""
import asyncio
import json
import os
import sys

from mcp.client.session import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

RAIZ = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(RAIZ, "target", "release", "rustranslator")


async def main() -> int:
    if not os.path.exists(BIN):
        print(f"FALHOU: o binario nao existe em {BIN} (rode cargo build --release)")
        return 1

    # env vazio de proposito: o servidor nao deve depender de nada do ambiente.
    params = StdioServerParameters(command=BIN, args=["mcp"], env={})

    async with stdio_client(params) as (leitura, escrita):
        async with ClientSession(leitura, escrita) as sessao:
            info = await sessao.initialize()
            print(f"handshake: {info.serverInfo.name} {info.serverInfo.version}")
            print(f"protocolVersion negociada: {info.protocolVersion}")

            ferramentas = await sessao.list_tools()
            nomes = [t.name for t in ferramentas.tools]
            print(f"tools: {len(nomes)} -> {nomes}")
            ESPERADAS = {"echo", "translate", "review", "submit", "status", "result", "cancel"}
            faltando = ESPERADAS - set(nomes)
            if faltando:
                print(f"FAILED: missing tools: {sorted(faltando)}")
                return 1

            texto = "prova da fase A — acentuacao e emoji: ção ✓"
            resultado = await sessao.call_tool("echo", {"text": texto})
            devolvido = resultado.content[0].text if resultado.content else ""
            print(f"echo: {devolvido!r}")

            if not devolvido.startswith(texto):
                print("FAILED: echo did not return the text it was sent")
                return 1
            if "echo at" not in devolvido:
                print("FAILED: the instant is missing — this could be a cached answer")
                return 1

            # A tool de progresso, contra o registro REAL de 262 blocos.
            print()
            golden = os.path.expanduser("~/.local/share/rustranslator/golden")
            prog = await sessao.call_tool(
                "status", {"document": "README", "dir": golden}
            )
            corpo = prog.content[0].text if prog.content else ""
            print("status (real registry):")
            print(corpo)

            if "262" not in corpo:
                print("FAILED: progress did not see the 262 blocks in the real registry")
                return 1
            if "227" not in corpo:
                print("FAILED: progress did not count the 227 approved")
                return 1

    print("\nOK — the Rust server answers a real MCP client (Tier 2).")
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
