#!/usr/bin/env bash
# Assert every HTTP route the gateway registers is listed in docs/reference/api-v1.md.
#
# Usage: scripts/ci/docs_api_route_coverage.sh
#
# Why this exists
# ---------------
# `docs/reference/api-v1.md` carries its own maintenance rule — "Every new
# `/api/v1` route must be added to this reference … as part of the same change
# that adds it". Nothing enforced it, and by 2026-09-07 seventeen registered
# routes were missing: the whole of `/api/v1/config/*`, `/api/v1/cron/*`,
# `/api/v1/secrets`, `/api/v1/memory/{key}`, `/api/v1/channels/telegram` and the
# five `/tasks*` routes.
#
# That is the same failure shape as the command-coverage backlog next door: a
# rule with no check is a preference. This is the check.
#
# Scope: `/api/v1/*` and `/tasks*`. The `/tasks` surface is not under `/api/v1`,
# but it is an HTTP API with no other reference page, so it is held to the same
# rule. Transport endpoints (`/webhook`, `/whatsapp`, `/health`, `/metrics`,
# `/pair`, `/login`, …) are deliberately out of scope — they are documented as
# operational surfaces elsewhere, not as a JSON API.
#
# How it works
# ------------
# Route names come from `.route("<path>", …)` calls under `src/gateway/`, which
# is where axum registers them. A route counts as documented when its literal
# path appears anywhere in api-v1.md.

set -euo pipefail

GATEWAY_DIR="src/gateway"
DOCS="docs/reference/api-v1.md"

if [ ! -d "$GATEWAY_DIR" ] || [ ! -f "$DOCS" ]; then
    echo "ERROR: expected $GATEWAY_DIR/ and $DOCS to exist (run from repo root)." >&2
    exit 2
fi

python3 - "$GATEWAY_DIR" "$DOCS" <<'PY'
import pathlib
import re
import sys

gateway_dir, docs_path = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])

routes: set[str] = set()
for path in sorted(gateway_dir.rglob("*.rs")):
    source = path.read_text()
    for match in re.finditer(r'\.route\(\s*"([^"]+)"', source, re.S):
        route = match.group(1)
        if route.startswith("/api/v1") or route.startswith("/tasks"):
            routes.add(route)

if not routes:
    print("ERROR: found no routes at all — the extraction is broken, not the docs.")
    sys.exit(2)

doc = docs_path.read_text()
missing = sorted(route for route in routes if route not in doc)

for route in missing:
    print(f"ERROR:   route `{route}` is registered in {gateway_dir}/ but absent from {docs_path}.")
    print("         Add it to the reference in the same change that adds the route.")

print(f"checked {len(routes)} route(s); {len(missing)} undocumented")

if missing:
    print()
    print("An HTTP route is undocumented. See the message(s) above.")
    sys.exit(1)
PY
