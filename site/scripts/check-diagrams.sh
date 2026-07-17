#!/bin/sh
set -eu

site_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
public_total=0
internal_total=0

check_accessibility() {
  awk '
    $0 == "```mermaid" {
      in_diagram = 1
      diagram += 1
      title = 0
      description = 0
      next
    }
    in_diagram && /^accTitle:[[:space:]]*[^[:space:]]/ { title = 1 }
    in_diagram && /^accDescr:[[:space:]]*[^[:space:]]/ { description = 1 }
    in_diagram && $0 == "```" {
      if (!title || !description) {
        printf "%s: Mermaid fence %d requires non-empty accTitle and accDescr\n", FILENAME, diagram > "/dev/stderr"
        invalid = 1
      }
      in_diagram = 0
    }
    END {
      if (in_diagram) {
        printf "%s: unclosed Mermaid fence %d\n", FILENAME, diagram > "/dev/stderr"
        invalid = 1
      }
      exit invalid
    }
  ' "$1"
}

for file in "$site_root"/content/docs/*.md; do
  count=$(awk '$0 == "```mermaid" { count += 1 } END { print count + 0 }' "$file")
  if [ "$count" -gt 3 ]; then
    echo "$file: public manual pages may contain at most 3 Mermaid diagrams (found $count)" >&2
    exit 1
  fi
  check_accessibility "$file"
  public_total=$((public_total + count))
done

for file in "$site_root"/content/internals/*.md; do
  count=$(awk '$0 == "```mermaid" { count += 1 } END { print count + 0 }' "$file")
  if [ "$count" -gt 6 ]; then
    echo "$file: architecture pages may contain at most 6 Mermaid diagrams (found $count)" >&2
    exit 1
  fi
  check_accessibility "$file"
  internal_total=$((internal_total + count))
done

total=$((public_total + internal_total))
if [ "$public_total" -ne 23 ] || [ "$internal_total" -ne 101 ] || [ "$total" -ne 124 ]; then
  echo "diagram inventory mismatch: Manual=$public_total Architecture=$internal_total total=$total; expected 23/101/124" >&2
  exit 1
fi

echo "Diagram governance passed: Manual=$public_total Architecture=$internal_total total=$total"
