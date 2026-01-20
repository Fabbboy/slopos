#!/usr/bin/env bash
set -euo pipefail

readme="README.md"
start="<!-- scc-table-start -->"
end="<!-- scc-table-end -->"

if ! command -v scc >/dev/null 2>&1; then
  echo "scc is not installed or not in PATH" >&2
  exit 1
fi

table_file="$(mktemp)"
trap 'rm -f "$table_file"' EXIT

scc --format=markdown > "$table_file"

awk -v start="$start" -v end="$end" -v table_file="$table_file" '
  $0 == start {
    print $0
    while ((getline line < table_file) > 0) {
      print line
    }
    close(table_file)
    in_block = 1
    next
  }
  $0 == end {
    in_block = 0
    print $0
    next
  }
  !in_block { print $0 }
' "$readme" > "${readme}.tmp"

mv "${readme}.tmp" "$readme"
