#!/usr/bin/env bash
# Regenerates docs/showcase/suggestions.png from synthetic sample text.
# Needs: pandoc, soffice (LibreOffice), pdftoppm, docx-suggest and docdiff on PATH.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/base.md" <<'M'
# Services Agreement

The Supplier shall deliver the Services within thirty (30) days of the Effective Date.

Payment is due within sixty (60) days of invoice.

Either party may terminate this Agreement on written notice.
M
cat > "$tmp/revised.md" <<'M'
# Services Agreement

The Supplier shall deliver the Services within fourteen (14) days of the Effective Date.

Payment is due within thirty (30) days of invoice.

Either party may terminate this Agreement on ninety (90) days' written notice.
M
cat > "$tmp/notes.json" <<'M'
[{"anchor": 1, "body": "Shortened delivery window. See https://example.com/policy", "links": []}]
M
pandoc "$tmp/base.md" -o "$tmp/base.docx"
pandoc "$tmp/revised.md" -o "$tmp/revised.docx"
docx-suggest "$tmp/base.docx" "$tmp/revised.docx" -o "$tmp/suggestions.docx" \
  --author "Sample Reviewer" --date 2026-01-01T00:00:00Z --comments "$tmp/notes.json"
soffice --headless --convert-to pdf --outdir "$tmp" "$tmp/suggestions.docx" >/dev/null
pdftoppm -png -r 80 -f 1 -l 1 -singlefile "$tmp/suggestions.pdf" "$here/suggestions"
