#!/usr/bin/env bash
# Assert every surface that shows a channel's labels agrees with the catalog it
# is supposed to be rendering, on BOTH axes.
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
# catalog key, that both static surfaces carry the same values the catalog does —
# the whole surface, not one sampled row.
#
# Two axes since 2026-09-08 (plan 327), and the gate checks both. A surface that
# renders `support` and forgets `verification` is the failure this exists for:
# one label answering two questions is what produced a document defining
# "supported" as "has been driven" three lines above a sentence saying one
# channel had been driven.
#
# FAIL-CLOSED: a catalog row with no matching docs row, or a mismatch on either
# axis, fails. There is no exception list; a label change is a two-file edit by
# design.

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
rows="$(grep -oE '\( *"[a-z_]+", *"[^"]+", *ChannelSupport::[A-Za-z]+, *ChannelVerification::[A-Za-z]+,? *\)' <<<"$catalog_block" \
  | tr -s ' ' || true)"

if [ -z "$rows" ]; then
  echo "ERROR: could not parse CHANNEL_CATALOG out of $CATALOG_SRC."
  echo "       Expected rows shaped"
  echo "         (\"key\", \"Label\", ChannelSupport::V, ChannelVerification::V)."
  exit 1
fi

declared="$(grep -oE 'ChannelVerification\); [0-9]+\]' <<<"$catalog_block" | grep -oE '[0-9]+' || true)"
parsed="$(wc -l <<<"$rows" | tr -d ' ')"
if [ "$declared" != "$parsed" ]; then
  echo "ERROR: CHANNEL_CATALOG declares $declared rows but $parsed parsed."
  echo "       A row this gate cannot read is a row it cannot check."
  exit 1
fi

# ── 2. Each label string is defined once, not typed per surface. ────────────
# One check per axis. A single combined count would pass while one axis grew a
# second copy and the other lost one.
for label in "under development" "not yet verified"; do
  label_defs="$(grep -rnF "\"$label\"" src/ --include='*.rs' | wc -l | tr -d ' ')"
  if [ "$label_defs" -ne 1 ]; then
    echo "ERROR: the string \"$label\" appears $label_defs times in src/."
    echo "       Each label is defined exactly once, in ChannelSupport::label() or"
    echo "       ChannelVerification::label(), and rendered from there. A second"
    echo "       copy is the drift this gate exists for:"
    grep -rnF "\"$label\"" src/ --include='*.rs'
    fail=1
  fi
done

# ── 3. Every catalog row matches both static surfaces. ──────────────────────
# The ChannelsConfig field docs only — a bare `pub email:` grep would find
# another struct's field and pass on the wrong evidence.
schema_block="$(awk '/^pub struct ChannelsConfig \{/,/^\}/' "$SCHEMA")"

supported=0
driven=0
checked=0
while IFS= read -r row; do
  key="$(sed -E 's/^\( *"([a-z_]+)".*/\1/' <<<"$row")"
  sup_variant="$(sed -E 's/.*ChannelSupport::([A-Za-z]+),.*/\1/' <<<"$row")"
  ver_variant="$(sed -E 's/.*ChannelVerification::([A-Za-z]+),? *\)$/\1/' <<<"$row")"
  case "$sup_variant" in
    Supported) support="supported"; supported=$((supported + 1)) ;;
    UnderDevelopment) support="under development" ;;
    *) echo "ERROR: $key has unknown support variant '$sup_variant'."; fail=1; continue ;;
  esac
  case "$ver_variant" in
    Driven) verification="verified"; driven=$((driven + 1)) ;;
    NotDriven) verification="not yet verified" ;;
    *) echo "ERROR: $key has unknown verification variant '$ver_variant'."; fail=1; continue ;;
  esac
  checked=$((checked + 1))

  if ! grep -qF "| \`$key\` | $support | $verification |" "$DOCS"; then
    echo "ERROR: $DOCS is stale for '$key'."
    echo "       The catalog says '$support' / '$verification'; no matching row."
    echo "       Expected a line: | \`$key\` | $support | $verification |"
    fail=1
  fi

  doc_line="$(grep -B1 "^    pub $key:" <<<"$schema_block" | head -1)"
  if ! grep -qF "Support: **$support**. Verification: **$verification**." <<<"$doc_line"; then
    echo "ERROR: $SCHEMA is stale for ChannelsConfig.$key."
    echo "       The catalog says '$support' / '$verification'; its doc comment reads:"
    echo "         $doc_line"
    echo "       The console renders this text, so a stale label ships to operators."
    fail=1
  fi
done <<<"$rows"

# ── 4. The two decisions themselves, counted separately. ────────────────────
# Separately because they are different facts with different owners. One
# combined assertion would pass while a channel moved from one axis to the
# other, which is the exact confusion this split removed.
#
# Four supported is the owner's 2026-09-04 product commitment.
if [ "$supported" -ne 4 ]; then
  echo "ERROR: $supported channels are marked Supported; the recorded decision is 4."
  echo "       The support axis is the OWNER's call, not a checklist outcome. If"
  echo "       the owner moved it, update this number and say which channel."
  fail=1
fi

# One driven is the evidence that exists. Telegram, #770.
if [ "$driven" -ne 1 ]; then
  echo "ERROR: $driven channels are marked Driven; the recorded evidence covers 1."
  echo "       The verification axis moves only when a round trip was actually run"
  echo "       against a real account and written down. See $DOCS §0."
  fail=1
fi

# ── 5. The wired count in the prose, derived rather than typed. ─────────────
# It said "Seventeen" against a sixteen-row catalog until 2026-09-08.
count_words="zero one two three four five six seven eight nine ten eleven twelve \
thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty"
expected_word="$(awk -v n="$checked" '{print $(n + 1)}' <<<"$count_words")"
expected_word="$(tr '[:lower:]' '[:upper:]' <<<"${expected_word:0:1}")${expected_word:1}"
if ! grep -qF "**$expected_word channels are wired." "$DOCS"; then
  echo "ERROR: $DOCS does not say \"$expected_word channels are wired.\""
  echo "       The catalog has $checked rows. That sentence is a count of them and"
  echo "       drifted once already; it is checked rather than trusted."
  grep -n "channels are wired" "$DOCS" || true
  fail=1
fi

echo "checked $checked catalog row(s) against $DOCS and $SCHEMA;" \
     "$supported supported, $driven driven"

if [ "$fail" -ne 0 ]; then
  echo
  echo "A channel label surface is out of sync with CHANNEL_CATALOG."
  exit 1
fi

exit 0
