#!/usr/bin/env bash
# Gera o pacote .deb do rustranslator a partir do binario de release.
#
# O QUE VAI NO PACOTE, e o que deliberadamente NAO vai:
#
#   vai     /usr/bin/rustranslator                     o binario
#           /usr/lib/systemd/user/rustranslator.service   o servico (do usuario, nao do sistema)
#           /usr/share/man/man1/rustranslator.1        o manual
#           /usr/share/doc/rustranslator/              README, LICENSE, config.example.json
#
#   NAO vai nenhum segredo. Nao existe token neste projeto — a configuracao de verdade mora em
#   ~/.config/rustranslator/config.json, do usuario, e o pacote so' instala o EXEMPLO, com o
#   campo api_key vazio.
#
# A arquitetura e' lida do sistema (dpkg --print-architecture), nao escrita a mao: um pacote
# rotulado amd64 com binario arm64 e' pior que um pacote ausente.
#
# Uso: bash packaging/build-deb.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
PKG="rustranslator_${VERSION}_${ARCH}"
BIN=target/release/rustranslator

if [ ! -x "$BIN" ]; then
    echo "erro: $BIN nao existe -- rode: cargo build --release" >&2
    exit 1
fi

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/rustranslator-deb.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT

install -Dm755 "$BIN"                            "$STAGE/usr/bin/rustranslator"
install -Dm644 packaging/rustranslator.service   "$STAGE/usr/lib/systemd/user/rustranslator.service"
install -Dm644 packaging/rustranslator.1         "$STAGE/usr/share/man/man1/rustranslator.1"
install -Dm644 packaging/config.example.json     "$STAGE/usr/share/doc/rustranslator/config.example.json"
install -Dm644 README.md                         "$STAGE/usr/share/doc/rustranslator/README.md"
install -Dm644 LICENSE                           "$STAGE/usr/share/doc/rustranslator/LICENSE"
gzip -9n -c packaging/rustranslator.1 > "$STAGE/usr/share/man/man1/rustranslator.1.gz"
rm -f "$STAGE/usr/share/man/man1/rustranslator.1"

mkdir -p "$STAGE/DEBIAN"
SIZE=$(du -sk "$STAGE/usr" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: rustranslator
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${SIZE}
Maintainer: Djair Guilherme da Silva Junior <djair.jr@gmail.com>
Homepage: https://github.com/djairjr/rustranslator
Description: Traducao de markdown com tradutor local e revisor em nuvem
 O modelo local escreve a traducao e um modelo em nuvem a revisa, de modo que
 so' o residuo marcado precise ser lido por uma pessoa. Um executavel so':
 traduz e serve como servidor MCP, nos transportes stdio e HTTP.
 .
 ESTA VERSAO E' UM ESQUELETO: o motor de traducao ainda nao foi portado. O que
 existe e' a configuracao do provedor, o servidor MCP e a estrutura de
 progresso da tarefa. Veja o README em /usr/share/doc/rustranslator/.
 .
 O pacote NAO liga o servico sozinho -- copie config.example.json para
 ~/.config/rustranslator/config.json, feche a permissao (chmod 600) e rode
 systemctl --user enable --now rustranslator.service.
EOF

dpkg-deb --root-owner-group --build "$STAGE" "$PKG.deb"
sha256sum "$PKG.deb" > "$PKG.deb.sha256"

echo "== construido =="
ls -la "$PKG.deb" "$PKG.deb.sha256"
echo
echo "== o que ha dentro =="
dpkg-deb --contents "$PKG.deb"
echo
echo "== e o controle =="
dpkg-deb --field "$PKG.deb" Package Version Architecture Installed-Size Maintainer
