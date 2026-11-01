#!/usr/bin/env bash
# Regenerates examples/sample/ from the text below (invented, generic sample).
# Needs pandoc. The .docx files are built with pandoc's default styles.
set -euo pipefail
out="$(cd "$(dirname "$0")" && pwd)/sample"
mkdir -p "$out"
cat > "$out/base.md" <<'M'
# Office Services Agreement

This Agreement is made on 12 March 2023 between Northwind Supplies ("Supplier") and Contoso Partners ("Client").

The Supplier will provide cleening services at the Client's premises every weekday.

The Client shall pay each invoice within sixty (60) days. Late payments may be charged interest at a rate the Supplier decides.

Either party may end this Agreement by giving the other written notice, and the Supplier will try to be reasonable about refunds.

Any dispute will be handled by the parties in good fath.
M
cat > "$out/revised.md" <<'M'
# Office Services Agreement

This Agreement is made on 12 March 2026 between Northwind Supplies ("Supplier") and Contoso Partners ("Client").

The Supplier will provide cleaning services at the Client's premises every weekday.

The Client shall pay each invoice within thirty (30) days. Late payments bear interest at 1% per month.

Either party may end this Agreement on thirty (30) days' written notice. On early termination the Supplier will refund any prepaid fees for the remaining period.

Any dispute will be handled by the parties in good faith.
M
cat > "$out/notes.json" <<'M'
[
  {"anchor": "12 March 2026", "body": "Year corrected from 2023."},
  {"anchor": "thirty (30) days. Late payments bear interest at 1% per month.", "body": "Reworded payment terms. Template: https://example.com/payment-terms", "links": []},
  {"anchor": "good faith", "body": "Spelling fix only."}
]
M
pandoc "$out/base.md" -o "$out/base.docx"
pandoc "$out/revised.md" -o "$out/revised.docx"
