#!/usr/bin/env bash
set -euo pipefail

NAME="${1:-world}"
COUNT=3

greet() {
  local who="$1"
  echo "Hello, ${who}!"
}

for i in $(seq 1 "$COUNT"); do
  if [[ "$i" -gt 1 ]] && [ -n "$NAME" ]; then
    greet "$NAME" | tr a-z A-Z
  fi
done

case "$NAME" in
  world) echo "default" ;;
  *) echo "custom" ;;
esac
cat <<EOT
heredoc $NAME
EOT
