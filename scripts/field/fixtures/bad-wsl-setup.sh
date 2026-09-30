#!/usr/bin/env bash
# Reconstruction of the pattern behind U07, U08 and U10 in the field-testing
# issue list: one set -e script for WSL that starts Ollama in the background
# with a wait that never fails, sets npm's prefix (breaks nvm) and ends by
# exec-ing the app. Written for the lint tests; not the original text.
set -euo pipefail
sudo apt-get update -y
sudo apt-get install -y curl zstd
command -v ollama >/dev/null || curl -fsSL https://ollama.com/install.sh | sh
nohup ollama serve > /tmp/ollama-serve.log 2>&1 &
for _ in $(seq 1 30); do curl -sf http://localhost:11434/api/version >/dev/null && break; sleep 1; done
ollama pull qwen3-vl:2b
npm config set prefix ~/.npm-global
export PATH="$HOME/.npm-global/bin:$PATH"
npm install -g buildwithnexus@latest
exec bwn --provider ollama --model qwen3-vl:2b
