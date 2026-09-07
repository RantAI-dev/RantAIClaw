# Security Policy

RantaiClaw is an autonomous agent runtime. It holds provider credentials, executes shell
commands, and can be reached over the network through its gateway and chat channels. Those
are the parts worth reporting on, and the parts we will act on fastest.

## Supported versions

| Version | Supported |
| --- | --- |
| The most recent published release | Yes |
| Anything older | No |

There are no maintained release branches. Every release so far is an `-alpha`, and fixes
ship forward in the next release rather than being backported. If you are running an older
version, the first step of any remediation will be to upgrade.

This is a deliberate limit, not an oversight: a single maintained line is what this project
can actually keep patched, and claiming more would be a promise it could not meet.

## Reporting a vulnerability

**Do not open a public issue for a vulnerability.** Public issues are indexed before anyone
has had a chance to fix anything.

Report privately through GitHub:

1. Go to the repository's [**Security** tab](https://github.com/RantAI-dev/RantAIClaw/security).
2. Choose **Report a vulnerability**.

That opens a private advisory visible only to you and the maintainers.

If the **Report a vulnerability** button is not present — the repository setting that enables
it can be toggled off — open a normal issue that contains **no technical detail**, titled
`security contact request`, and a maintainer will open a private advisory and invite you to
it. Say nothing about the vulnerability in that issue beyond the fact that you have one.

### What to include

The more of this you can give, the faster it moves:

- affected version (`rantaiclaw --version`) and platform
- which surface is involved: gateway, a chat channel, a tool, the TUI, the web console, config or secret handling
- what an attacker can do, and what access they need first
- reproduction steps, or a proof of concept
- any log output, **with tokens and keys redacted**

### What to expect

Honest version: this is a small project, maintained by one person, in alpha.

- We aim to acknowledge a report within **7 days**. If you have heard nothing in 14, please
  comment on the advisory — it means it was missed, not ignored.
- Once acknowledged, we will tell you whether we can reproduce it and what we intend to do.
- If we accept a fix, we will tell you which release will carry it. If we decline, we will
  tell you why rather than letting the report go quiet.
- We are happy to credit you in the release notes and the advisory. Say if you would rather
  not be named.

No bug-bounty programme, and no payment. Nothing here is a legal commitment or a service
level agreement.

## Scope

**In scope** — these are the boundaries the project treats as security-relevant:

- **Exposure surfaces**: gateway bind address and pairing, webhook authentication, rate limits, tunnel handling. These are deny-by-default by design; a way past them is a vulnerability.
- **Credential and secret handling**: the encrypted secret store, key file permissions, credentials reaching logs, transcripts, error messages or telemetry.
- **Sandbox and tool boundaries**: escaping the shell-tool allowlist, path traversal out of the workspace, a tool obtaining capability it was not granted.
- **Approval and autonomy**: any path that executes an action the configured autonomy level should have required approval for.
- **Channel authorisation**: impersonating an owner, or a non-owner reaching owner-only capability.
- **Supply chain**: our release artefacts, their signatures, and the workflows that produce them.
- **Prompt injection that crosses a trust boundary** — untrusted content (a web page, an email, a message from a non-owner) causing the agent to take a privileged action or exfiltrate a secret.

**Out of scope**:

- Capability the operator deliberately enabled. Local tools ship on by default — shell execution, `http_request` to any domain, web search, the browser. An agent running shell commands on the machine you asked it to run on is the product, not a vulnerability. What *is* in scope is a path around the limits you configured.
- Anything requiring an attacker who already has local access to the machine or the config directory. If they can read `config.toml`, they have the keys, and no design here defends against that.
- Findings from a scanner with no demonstrated impact, missing hardening headers with no exploit, and denial of service by simply sending more traffic.
- Vulnerabilities in a third-party dependency with no path through this codebase — report those upstream. If there *is* a path through our usage, we want to hear about it.
- Anything in a release that is not the most recent one, unless it is still present in the current release.

## Disclosure

We ask for **90 days** from acknowledgement before public disclosure, or until a fixed
release ships, whichever comes first. If we go quiet, or a fix drifts past that window
without explanation, publish — a report the maintainer sat on is not your problem to carry.

When a fix ships, the release notes name the issue and the advisory is published. The
`CHANGELOG.md` `### Security` sections are the running record of that.

## Hardening

If you are deciding how to run this, the relevant documents are
[`docs/security/`](docs/security/) for the security model and
[`docs/operations/runbook.md`](docs/operations/runbook.md) for deployment. The short version:
the gateway binds to localhost and requires pairing, `allow_public_bind` defaults to `false`,
and those defaults exist for a reason.
