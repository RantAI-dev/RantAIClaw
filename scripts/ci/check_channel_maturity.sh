#!/usr/bin/env bash
# Assert every surface that shows a channel's maturity tier agrees with the
# catalog it is supposed to be rendering.
#
# Usage: check_channel_maturity.sh
#
# Why this exists
# ---------------
# The owner named four supported channels on 2026-09-04. That decision now lives
# in `CHANNEL_CATALOG` (src/channels/mod.rs) and the runtime surfaces — the API,
# `channel list`, `status`, the TUI panels, `doctor` — read it from there, so
# they cannot drift.
#
# Two surfaces cannot read it at runtime: the table in docs/reference/channels.md
# and the config-schema doc comments the console renders. Those are hand-written
# text, and hand-written copies of this catalog are exactly the failure mode:
# claw-ui carried a second hand-synced CHANNEL_CATALOG whose own comment admitted
# the drift risk, and /api/v1/channels shipped for months reporting 7 of 11
# channels because a hand-written list was never updated.
#
# So this gate does not check that the docs "mention" the tiers. It checks, per
# catalog key, that both static surfaces carry the same tier the catalog does —
# the whole surface, not one sampled row.
#
# FAIL-CLOSED: a catalog row with no matching docs row, or a mismatched tier,
# fails. There is no exception list; a tier change is a two-file edit by design.

set -euo pipefail

CATALOG_SRC="src/channels/mod.rs"
DOCS="docs/reference/channels.md"
SCHEMA="src/config/schema.rs"

for f in "$CATALOG_SRC" "$DOCS" "$SCHEMA"; do
  [ -f "$f" ] || { echo "ERROR: $f not found (run from the repo root)"; exit 1; }
done

fail=0

# ── 1. The catalog is the source. Read it. ──────────────────────────────────
# Comments are stripped and the block is folded to one line before matching, so
# a row rustfmt wrapped over four lines reads the same as a row that fits on
# one. Requiring a formatting shape here would make `cargo fmt` and this gate
# disagree, and the one that loses that argument is always the gate.
catalog_block="$(awk '/^pub\(crate\) const CHANNEL_CATALOG/,/^\];/' "$CATALOG_SRC" \
  | sed -E 's://.*::' \
  | tr '\n' ' ' \
  | sed -E 's/[[:space:]]+/ /g')"
rows="$(grep -oE '\( *"[a-z_]+", *"[^"]+", *ChannelMaturity::[A-Za-z]+,? *\)' <<<"$catalog_block" \
  | tr -s ' ' || true)"

if [ -z "$rows" ]; then
  echo "ERROR: could not parse CHANNEL_CATALOG out of $CATALOG_SRC."
  echo "       Expected rows shaped (\"key\", \"Label\", ChannelMaturity::Variant)."
  exit 1
fi

declared="$(grep -oE 'ChannelMaturity\); [0-9]+\]' <<<"$catalog_block" | grep -oE '[0-9]+' || true)"
parsed="$(wc -l <<<"$rows" | tr -d ' ')"
if [ "$declared" != "$parsed" ]; then
  echo "ERROR: CHANNEL_CATALOG declares $declared rows but $parsed parsed."
  echo "       A row this gate cannot read is a row it cannot check."
  exit 1
fi

# ── 2. The label string is defined once, not typed per surface. ─────────────
label_defs="$(grep -rn '"under development"' src/ --include='*.rs' | wc -l | tr -d ' ')"
if [ "$label_defs" -ne 1 ]; then
  echo "ERROR: the string \"under development\" appears $label_defs times in src/."
  echo "       It must be defined exactly once, in ChannelMaturity::label(), and"
  echo "       rendered from there. A second copy is the drift this gate exists for:"
  grep -rn '"under development"' src/ --include='*.rs'
  fail=1
fi

# ── 3. Every catalog row matches both static surfaces. ──────────────────────
# The ChannelsConfig field docs only — a bare `pub email:` grep would find
# another struct's field and pass on the wrong evidence.
schema_block="$(awk '/^pub struct ChannelsConfig \{/,/^\}/' "$SCHEMA")"

supported=0
checked=0
while IFS= read -r row; do
  key="$(sed -E 's/^\( *"([a-z_]+)".*/\1/' <<<"$row")"
  variant="$(sed -E 's/.*ChannelMaturity::([A-Za-z]+),? *\)$/\1/' <<<"$row")"
  case "$variant" in
    Supported) tier="supported"; supported=$((supported + 1)) ;;
    UnderDevelopment) tier="under development" ;;
    *) echo "ERROR: $key has unknown maturity variant '$variant'."; fail=1; continue ;;
  esac
  checked=$((checked + 1))

  if ! grep -qF "| \`$key\` | $tier |" "$DOCS"; then
    echo "ERROR: $DOCS is stale for '$key'."
    echo "       The catalog says '$tier'; the maturity table has no matching row."
    echo "       Expected a line: | \`$key\` | $tier |"
    fail=1
  fi

  doc_line="$(grep -B1 "^    pub $key:" <<<"$schema_block" | head -1)"
  if ! grep -qF "Maturity: **$tier**." <<<"$doc_line"; then
    echo "ERROR: $SCHEMA is stale for ChannelsConfig.$key."
    echo "       The catalog says '$tier'; its doc comment reads:"
    echo "         $doc_line"
    echo "       The console renders this text, so a stale tier ships to operators."
    fail=1
  fi
done <<<"$rows"

# ── 4. The tier decision itself. ────────────────────────────────────────────
# Four is the owner's 2026-09-04 decision. Promotion is plan 321's job and needs
# evidence in the PR body, so a fifth appearing without one is caught here.
if [ "$supported" -ne 4 ]; then
  echo "ERROR: $supported channels are marked Supported; the recorded decision is 4."
  echo "       Promoting a channel needs the checklist in $DOCS (§0) — a driven"
  echo "       round trip on a real account — and a doctor probe. If that happened,"
  echo "       update this number here and say which channel and what was driven."
  fail=1
fi

echo "checked $checked catalog row(s) against $DOCS and $SCHEMA; $supported supported"

if [ "$fail" -ne 0 ]; then
  echo
  echo "A channel maturity surface is out of sync with CHANNEL_CATALOG."
  exit 1
fi

exit 0
