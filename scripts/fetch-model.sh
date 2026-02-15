#!/usr/bin/env bash
#
# Download the tokenizer and chat template for the model the project targets.
#
# Only these two files are needed. The router never runs the model; it needs to
# cut prompts into the same tokens and the same blocks the worker will, and that
# is entirely decided by the tokenizer and the chat template. The weights are
# vLLM's problem, at R0.5.
#
# Qwen3-1.7B is ungated and Apache-2.0, so no Hugging Face token is required.

set -euo pipefail

cd "$(dirname "$0")/.."

MODEL=${MODEL:-Qwen/Qwen3-1.7B}
DEST=${DEST:-.cache/qwen3-1.7b}

mkdir -p "$DEST"

for file in tokenizer.json tokenizer_config.json; do
  echo "fetching ${file}"
  curl -sSL --fail -o "${DEST}/${file}" \
    "https://huggingface.co/${MODEL}/resolve/main/${file}"
done

echo
echo "${MODEL} tokenizer and chat template are in ${DEST}"
ls -lh "$DEST"
