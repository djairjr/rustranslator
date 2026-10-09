#!/usr/bin/env bash
# Instala o rustranslator no sistema e liga o servico MCP no systemd do usuario.
#
# Duas metades, de proposito:
#   * o BINARIO vai para /usr/local/bin — do sistema, qualquer usuario executa (precisa de root);
#   * o SERVICO e' um unit do USUARIO — ele le ~/.config/rustranslator/config.json, onde esta'
#     o token. Um servico de sistema rodaria como root e nao acharia a config de ninguem.
#
# Uso: bash packaging/install-systemwide.sh
#
# NOTA sobre o pkexec: ele NAO leva timeout. Se pedir autenticacao, o prompt fica esperando
# voce — e se nao houver resposta, a operacao para e deve ser repetida, nao contornada.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

BIN=target/release/rustranslator
DESTINO=/usr/local/bin/rustranslator
UNIT_DIR="$HOME/.config/systemd/user"
UNIT="$UNIT_DIR/rustranslator.service"

echo "== 1. build =="
cargo build --release
[ -x "$BIN" ] || { echo "erro: $BIN nao existe" >&2; exit 1; }

echo
echo "== 2. instalando o binario em $DESTINO (precisa de root) =="
# Sem timeout: se o pkexec pedir autenticacao, ele espera por voce.
pkexec install -Dm755 "$BIN" "$DESTINO"
echo "  ok: $(ls -la "$DESTINO" | awk '{print $5}') bytes"

echo
echo "== 3. unit do usuario em $UNIT =="
mkdir -p "$UNIT_DIR"
if [ ! -f "$UNIT" ]; then
    echo "  ATENCAO: $UNIT nao existe — copie o unit do repositorio antes de continuar." >&2
    exit 1
fi
systemctl --user daemon-reload
systemctl --user enable --now rustranslator.service
echo "  ok"

echo
echo "== 4. o servico esta no ar? =="
sleep 1
systemctl --user --no-pager status rustranslator.service | head -6 || true
echo
echo "== 5. o endpoint responde? =="
curl -s -o /dev/null -w "  HTTP %{http_code} em http://127.0.0.1:9720/mcp\n" \
    -H "Accept: application/json, text/event-stream" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
    http://127.0.0.1:9720/mcp || echo "  (sem resposta)"

echo
echo "== 6. e o proprio Hermes ve? =="
hermes mcp test rustranslator 2>&1 | head -8 || true
