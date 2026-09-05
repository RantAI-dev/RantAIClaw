# Plan 152: claw-ui — expected-Host guard locks LAN operators out; restore IP access, say why when blocked

> **Executor instructions**: This plan is for the **claw-ui repo**
> (`~/project/rantai/claw-ui`), plus one docs file and one version pin in
> RantAIClaw. Steps 1 and 2 are one PR (one concern: the host guard's
> operator-facing behaviour). Run every verification command, including the
> live curl drive in step 4 — the 403 must be reproduced **before** the fix
> and re-checked **after** (probe needs a control). If anything in "STOP
> conditions" occurs, stop and report. When done, add this plan's row in
> `plans/README.md`.
>
> **Drift check (run first, in claw-ui)**:
> `git diff --stat v0.3.18..HEAD -- src/lib/request-origin.ts src/proxy.ts src/components/console/chat-pane.tsx src/lib/bff-confinement.test.ts`
> All line numbers below are from claw-ui `2150681` (= tag `v0.3.18`). If this
> diff is non-empty, re-verify each cited line before editing.

## Status

- **Priority**: P1 — upgrading to claw-ui v0.3.18 silently locks out any operator who opens the console via a LAN IP, and the error tells them to restart a gateway that is running fine
- **Effort**: S
- **Risk**: MEDIUM (narrows a security guard; the narrowing is argued below and pinned by tests)
- **Depends on**: none
- **Category**: bugfix (claw-ui) + docs (RantAIClaw)
- **Planned at**: claw-ui `2150681` (v0.3.18), RantAIClaw `9981a35`, 2026-08-16

## Why this matters

Reproduced live by an operator on 2026-08-16:

1. Console open at `http://192.168.18.170:3939/chat` — worked for weeks.
2. Upgrade to claw-ui v0.3.18.
3. Every panel dies with: `Gateway unreachable. Start the agent gateway, then
   retry — unexpected_host.` The gateway is up; restarting it changes nothing.

The failing chain:

- `src/proxy.ts:64-72` — every `/api/rc/*` request's `Host` header is checked
  against `expectedHosts()`; misses get 403 `{"error":"unexpected_host"}`.
- `src/lib/request-origin.ts:92` — the default allowlist is loopback only:
  `localhost`, `127.0.0.1`, `[::1]`, `::1`. A LAN IP is not in it, and nothing
  in `rantaiclaw ui start` sets `RANTAICLAW_UI_ALLOWED_HOSTS` /
  `RANTAICLAW_UI_DEV_ORIGINS` (RantAIClaw `src/webui.rs` only passes
  `RANTAICLAW_UI_SECRET`).
- `src/hooks/use-gateway-status.ts:32-34` — the status poll's 403 flips
  `connection` to `"offline"` with `error = "unexpected_host"`, and
- `src/components/console/chat-pane.tsx:103` — `ConnectionBanner` renders every
  offline state as "Gateway unreachable. Start the agent gateway…".

Two distinct defects:

1. **Silent lockout on upgrade.** The guard's own doc comment
   (`request-origin.ts:85-86`) says "a lockout that looks like an outage is its
   own failure" — and then the default allowlist produces exactly that lockout
   for LAN operators.
2. **Dishonest error.** A 403 from the console's *own middleware* is labelled
   as the gateway being down, pointing the operator at the wrong subsystem.

## Why the guard exists — do NOT revert it

Added in claw-ui `c23d2e9` (#60, first shipped in v0.3.18), closing a
DNS-rebinding hole found in the 2026-08-12 channels deepscan. The BFF signs
every `/api/rc/*` request with the gateway bearer token, so "the request
arrived" already means "privileged". The CSRF check compares `Origin` against
the request's own `Host` — correct and bind-address-independent, but a rebound
DNS name defeats it: the attacker serves a page at `evil.test`, points
`evil.test`'s DNS at the console's address, and the browser then sends
`Origin: http://evil.test:3939` + `Host: evil.test:3939`. Both agree, the page
becomes "same-origin" with the console, and its script can read the full
config and issue privileged writes. The Host allowlist is what blocks that.

## The design insight that satisfies both goals

**DNS rebinding requires a DNS name.** The attack works by pointing a *name*
the attacker controls at the console's address; the browser then sends that
name in `Host`. A request whose `Host` is an **IP literal** cannot be a
rebinding request — the browser was pointed at the address itself, resolved
nothing, and connected straight to the console. A cross-site `fetch()` aimed
at the IP from an attacker page is a different attack, and the existing
`isCrossSiteWrite` (`Sec-Fetch-Site`/`Origin` check, `proxy.ts:48-57`) plus
the browser's same-origin policy already handle it.

Therefore: **accept any IP-literal Host; keep the name allowlist for DNS
names.** Which IP addresses the console is reachable on is the bind address's
decision (`--host` on `ui start`), not this gate's — restricting to private
ranges here would add zero rebinding protection while recreating the lockout
for VPN/WireGuard operators. This restores pre-v0.3.18 behaviour for every
IP-based access path. Name-based access (tunnel domains, `console.lan`) still
requires `RANTAICLAW_UI_ALLOWED_HOSTS`, which is exactly the rebindable class.

## Step 1 — accept IP-literal Hosts (claw-ui)

**Modify `src/lib/request-origin.ts:116-121`** — replace `isUnexpectedHost`
with:

```ts
/** Whether `hostname` (port already stripped) is an IP literal. */
function isIpLiteral(hostname: string): boolean {
  // IPv4: exactly four octets, each 0-255. `127.0.0.1.evil.test` has five
  // dot-parts and fails the shape check — a name that merely *contains* an IP
  // is still a name.
  const v4 = hostname.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/);
  if (v4) return v4.slice(1).every((o) => Number(o) <= 255);
  // IPv6 arrives bracketed in a Host header (`[::1]`, `[fe80::1]`). Bare
  // colon form is accepted too — it costs nothing and `::1` is already in
  // the default set.
  const v6 = hostname.startsWith("[") && hostname.endsWith("]")
    ? hostname.slice(1, -1)
    : hostname;
  return v6.includes(":") && /^[0-9a-fA-F:.]+$/.test(v6);
}

/**
 * Whether the `Host` this request arrived on is one the console serves.
 *
 * A missing Host is refused: every browser sends one, and something that does
 * not is not a context this gate exists to protect.
 *
 * An IP-literal Host is always served: DNS rebinding — the attack this gate
 * blocks — works by pointing a *name* at the console's address, and the
 * browser then sends that name as Host. A literal IP means the browser was
 * pointed at the address itself. Which addresses the console answers on is
 * the bind address's decision, not this gate's.
 */
export function isUnexpectedHost(host: string | null, allowed: string[]): boolean {
  if (!host) return true;
  // Compare on the hostname alone — the port is the console's own and varies.
  const hostname = host.replace(/:\d+$/, "");
  if (isIpLiteral(hostname)) return false;
  return !allowed.some((a) => a === host || a === hostname || a.replace(/:\d+$/, "") === hostname);
}
```

**Modify `src/lib/bff-confinement.test.ts:163-200`** — extend the
`"expected-Host allowlist"` describe block:

- Add:

```ts
it("serves any IP-literal Host without configuration — rebinding needs a name", () => {
  for (const host of [
    "192.168.18.170:3939",
    "10.0.0.5",
    "[::1]:3939",
    "[fe80::1]:3939",
  ]) {
    expect(isUnexpectedHost(host, loopback)).toBe(false);
  }
});

it("does not mistake a name containing an IP for an IP literal", () => {
  expect(isUnexpectedHost("127.0.0.1.evil.test:3939", loopback)).toBe(true);
  expect(isUnexpectedHost("999.1.1.1", loopback)).toBe(true);
});
```

- Update the `"honours the operator's configured hosts"` test (line 186-195):
  `192.168.1.20:3939` now passes without any env — change that assertion's
  fixture to a *name* so the test keeps pinning what the env vars are for:

```ts
it("honours the operator's configured hosts, bare or as an origin", () => {
  const allowed = expectedHosts({
    devOrigins: "http://console.dev.lan:3939",
    allowedHosts: "console.lan",
  });
  expect(isUnexpectedHost("console.dev.lan:3939", allowed)).toBe(false);
  expect(isUnexpectedHost("console.lan:3939", allowed)).toBe(false);
  // Still not anything else.
  expect(isUnexpectedHost("evil.test", allowed)).toBe(true);
});
```

- The rebinding test at line 166-178 (`evil.test` rejected) must stay green
  untouched — it is the guard's reason to exist.

**Verify**: `pnpm vitest run src/lib/bff-confinement.test.ts` — all pass.
Run it once BEFORE editing too: the new IP-literal test must FAIL against
v0.3.18 (proves the test bites).

## Step 2 — honest banner when still blocked (claw-ui)

After step 1 this state is only reachable via an unlisted DNS name (tunnel
domain, mDNS name) — rarer, but the message must stop blaming the gateway.

**Modify `src/components/console/chat-pane.tsx:78-108`** — `ConnectionBanner`
gets a third variant. The component only renders client-side (banner appears
after the status poll fails, `chat-pane.tsx:304`), so `window` is safe:

```tsx
function ConnectionBanner({
  needsAuth,
  error,
}: {
  needsAuth: boolean;
  error: string | null;
}) {
  // The BFF's own Host-allowlist 403 (`proxy.ts`), not a gateway failure —
  // telling the operator to restart the gateway would point them at the
  // wrong subsystem entirely.
  const blockedHost = error?.includes("unexpected_host")
    ? window.location.hostname
    : null;
  return (
    <div /* className unchanged */>
      {/* icon block unchanged */}
      <span>
        {needsAuth ? (
          <>Gateway requires pairing — register a token, then restart the daemon.</>
        ) : blockedHost ? (
          <>
            Console reached via unlisted host “{blockedHost}”. Add it to
            RANTAICLAW_UI_ALLOWED_HOSTS and restart the console, or open via
            localhost.
          </>
        ) : (
          <>Gateway unreachable. Start the agent gateway, then retry{error ? ` — ${error}` : ""}.</>
        )}
      </span>
    </div>
  );
}
```

Export `ConnectionBanner` (add `export`) and add
`src/components/console/connection-banner.test.tsx` using the component-test
harness stood up in #60 (`harness.test.tsx` proves jsdom + testing-library
work):

```tsx
import { render, screen } from "@testing-library/react";
import { ConnectionBanner } from "./chat-pane";

describe("ConnectionBanner", () => {
  it("labels the BFF host rejection as a host problem, not a gateway outage", () => {
    render(<ConnectionBanner needsAuth={false} error="unexpected_host" />);
    expect(screen.getByText(/unlisted host/)).toBeInTheDocument();
    expect(screen.getByText(/RANTAICLAW_UI_ALLOWED_HOSTS/)).toBeInTheDocument();
    expect(screen.queryByText(/Start the agent gateway/)).toBeNull();
  });

  it("keeps the outage wording for a real connection failure", () => {
    render(<ConnectionBanner needsAuth={false} error="fetch failed" />);
    expect(screen.getByText(/Gateway unreachable/)).toBeInTheDocument();
  });

  it("keeps the pairing wording when auth is the problem", () => {
    render(<ConnectionBanner needsAuth={true} error="401" />);
    expect(screen.getByText(/requires pairing/)).toBeInTheDocument();
  });
});
```

**Verify**: `pnpm vitest run` — full suite green.

## Step 3 — docs + changelog

- **claw-ui `README.md`**: in the `RANTAICLAW_UI_ALLOWED_HOSTS` section, state
  the v0.3.19 behaviour: IP-literal Hosts are always served; the env var is
  for DNS names (tunnel domains, `.lan` names) only.
- **claw-ui `CHANGELOG.md`**: fix entry — "v0.3.18's expected-Host guard
  locked out operators reaching the console by IP; IP literals are now always
  served (rebinding requires a DNS name), and a host rejection is reported as
  such instead of as a gateway outage."
- **RantAIClaw `docs/start/troubleshooting.md`**: add an entry keyed on the
  literal symptom string `unexpected_host`: cause (console Host allowlist, not
  the gateway), fix (upgrade UI to ≥ v0.3.19 for IP access; set
  `RANTAICLAW_UI_ALLOWED_HOSTS=<name>` before `rantaiclaw ui start` for DNS
  names). This is a runtime-contract doc (CLAUDE.md §4.1) — keep it factual.

## Step 4 — live verification (both directions), release

Build honestly (traps from memory: `rtk next build` can fake success; `next
start` serves stale standalone — run the standalone server directly):

```bash
cd ~/project/rantai/claw-ui && pnpm build
node .next/standalone/server.js &   # console on :3939, gateway running
```

Control (BEFORE fix, on v0.3.18) and treatment (AFTER):

```bash
# LAN-IP Host — 403 {"error":"unexpected_host"} before, 200 after:
curl -si -H "Host: 192.168.18.170:3939" http://127.0.0.1:3939/api/rc/status | head -3
# Rebound-name Host — 403 before AND after (guard intact):
curl -si -H "Host: evil.test:3939" http://127.0.0.1:3939/api/rc/status | head -3
```

Then a browser drive from the LAN IP (`http://192.168.18.170:3939/chat`): chat
sends and streams. Banner check: add a hosts-file alias (`console.test →
127.0.0.1`), open `http://console.test:3939/chat`, confirm the banner reads
"unlisted host" — not "Start the agent gateway".

**Release**: tag claw-ui `v0.3.19`, then bump the UI version pin in RantAIClaw
(find it: `rtk proxy grep -rn "0\.3\.18" src/` in RantAIClaw — the `ui
update`/prebuilt-download path pins it) and land the troubleshooting doc in the
same RantAIClaw PR. Two-repo release ordering per memory: claw-ui tag first,
then pin bump.

## STOP conditions

- Drift check non-empty and any cited function moved or changed shape.
- The pre-fix control curl does NOT return 403 on v0.3.18 — the repro chain is
  different from this plan's analysis; re-diagnose before editing.
- `evil.test` curl returns 200 at any point after the change — the guard is
  broken; do not ship.
- Component-test harness can't render (`connection-banner.test.tsx` fails on
  infra, not assertions): keep step 1, ship the banner change with the message
  logic extracted into a pure exported function
  `bannerMessage(needsAuth: boolean, error: string | null, hostname: string): string`
  tested in plain vitest instead.

## Rollback

Single revert of the claw-ui PR restores the v0.3.18 guard exactly (loopback +
env allowlist, generic banner). The RantAIClaw doc/pin PR reverts
independently. No config schema, no gateway, no stored state touched.
