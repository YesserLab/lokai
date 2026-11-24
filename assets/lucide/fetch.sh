#!/bin/sh
# Downloads the Lucide icons LoKI uses from lucide.dev (ISC license, see LICENSE).
set -e
cd "$(dirname "$0")"
for name in settings trash folder-open check x ban triangle-exclamation-point circle-exclamation-point copy arrow-up square \
    chevron-right chevron-down loader-circle bot-message-square; do
    curl -fsSL "https://lucide.dev/api/icons/$name" -o "$name.svg"
done
curl -fsSL https://raw.githubusercontent.com/lucide-icons/lucide/main/LICENSE -o LICENSE
