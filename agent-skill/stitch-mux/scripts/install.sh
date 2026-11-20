#!/bin/sh
# Idempotently install the local-only stitch-mux skill into known agent skill
# directories. No network, no privileged hooks, no modifications outside the
# selected user-level skill roots.
set -eu

SOURCE_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ROOT=${STITCH_MUX_SKILL_ROOT:-$HOME}
TARGETS=${STITCH_MUX_SKILL_TARGETS:-claude,codex,opencode,pi}

for target in $(printf '%s' "$TARGETS" | tr ',' ' '); do
  case "$target" in
    claude)   dest="$ROOT/.claude/skills/stitch-mux" ;;
    codex)    dest="$ROOT/.codex/skills/stitch-mux" ;;
    opencode) dest="$ROOT/.config/opencode/skills/stitch-mux" ;;
    pi)       dest="$ROOT/.agents/skills/stitch-mux" ;;
    *)
      echo "unknown skill target: $target" >&2
      exit 2
      ;;
  esac

  mkdir -p "$dest/references"
  cp "$SOURCE_DIR/SKILL.md" "$dest/SKILL.md"
  cp "$SOURCE_DIR/references/commands.md" "$dest/references/commands.md"
  echo "installed stitch-mux skill: $dest"
done
