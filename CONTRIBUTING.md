# Contributing to RantaiClaw

Thanks for helping build RantaiClaw. This page is the entry point: it covers the
flow every contribution follows and points at the detailed documents for each
step. It intentionally stays short — the authoritative rules live in the linked
files and are kept current there.

- **Engineering protocol:** [`CLAUDE.md`](CLAUDE.md) — principles, risk tiers, change playbooks
- **Agent instructions:** [`AGENTS.md`](AGENTS.md)
- **PR workflow (authoritative):** [`docs/contributing/pr-workflow.md`](docs/contributing/pr-workflow.md)
- **Reviewer playbook:** [`docs/contributing/reviewer-playbook.md`](docs/contributing/reviewer-playbook.md)
- **CI map:** [`docs/contributing/ci-map.md`](docs/contributing/ci-map.md)
- **Release process:** [`docs/contributing/release-process.md`](docs/contributing/release-process.md)
- **Actions source policy:** [`docs/contributing/actions-source-policy.md`](docs/contributing/actions-source-policy.md)

## Ground rules

1. **Work on a branch, never push to `main`.** Open a PR and let the required
   checks run.
2. **One concern per PR.** Do not mix a feature, a refactor, and an infra change
   in one patch — it blocks safe rollback.
3. **You are accountable for what you submit.** Agent-assisted PRs are welcome,
   but you must understand what the code does before requesting review.
4. **Never commit secrets or personal data.** No real names, emails, tokens, API
   keys, or private URLs — in code, docs, tests, fixtures, or commit messages.
   Use neutral placeholders (`test_user`, `example.com`, `rantaiclaw_bot`).

## Getting set up

```bash
git clone https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
cargo build
```

Default features are `tui`, `whatsapp-web`, `remote-install`, and `kb`. See the
[README](README.md#feature-flags) for the optional ones.

## The flow

### 1. Branch

Use a descriptive, scoped branch name (`fix/telegram-reply-split`,
`docs/config-reference`). For concurrent tracks, use one git worktree per branch
so unrelated edits never mix.

### 2. Change

Keep the patch minimal and match the surrounding style. Extend behavior by
implementing an existing trait and registering it in the relevant factory rather
than rewriting across modules — see the change playbooks in
[`CLAUDE.md`](CLAUDE.md) §7 for providers, channels, tools, and peripherals.

Know your risk tier, because it sets how much validation is expected:

| Tier | Paths | Expectation |
|---|---|---|
| Low | docs, chores, tests only | markdown lint + link check |
| Medium | most `src/**` behavior changes | relevant tests, focused scenarios |
| High | `src/security/**`, `src/runtime/**`, `src/gateway/**`, `src/tools/**`, `.github/workflows/**` | at least one boundary/failure-mode validation, plus threat and rollback notes |

When uncertain, treat it as the higher tier.

### 3. Validate locally

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Or run the Docker-based CI mirror if Docker is available:

```bash
./dev/ci.sh all
```

You are not blocked from opening a PR when local Docker CI is unavailable — run
the most relevant subset and say in the PR what you ran and what you skipped.

If you touched the bootstrap scripts, also run
`bash -n bootstrap.sh scripts/bootstrap.sh scripts/install.sh`.

### 4. Commit

Use conventional-commit subjects (`feat:`, `fix:`, `docs:`, `chore:`,
`refactor:`, `test:`, `ci:`), optionally scoped: `fix(telegram): …`. PR titles
are linted against the same rule.

If your PR supersedes someone else's and carries their work forward, add a
`Co-authored-by:` trailer per contributor — see [`CLAUDE.md`](CLAUDE.md) §9.2.

### 5. Open the PR

Fill in [the PR template](.github/pull_request_template.md) completely. The
required sections — validation evidence, security impact, privacy, blast radius,
rollback plan — are checked at intake, not decoration.

Keep PRs small: `XS` ≤ 80 changed lines, `S` ≤ 250, `M` ≤ 500. Target `XS`/`S`/`M`.
Anything `L`/`XL` needs explicit justification and tighter test evidence; split
large features into stacked PRs and declare `Depends on #…`.

Before requesting review, confirm the scope boundary is explicit, validation
evidence is attached (not just "CI will check"), and rollback is concrete.

### 6. Merge

`CI Required Gate` aggregates the Rust quality stages and must be green.
CODEOWNERS approval is required on owned paths. Risk labels must match the paths
you touched. Merge through the PR controls — do not push to `main`.

## Reporting bugs and requesting features

Open an [issue](https://github.com/RantAI-dev/RantAIClaw/issues). For bugs,
include your OS, `rantaiclaw --version`, the command you ran, and what you
expected versus what happened. Redact tokens and private URLs first.

**Security vulnerabilities do not belong in public issues.** Report them
privately through the [security policy](https://github.com/RantAI-dev/RantAIClaw/security/policy).

## Documentation changes

Documentation is a product surface, not an afterthought. If you change CLI,
config, provider, or channel behavior, update the matching runtime-contract
reference in the same PR:

- [`docs/reference/commands.md`](docs/reference/commands.md)
- [`docs/reference/config.md`](docs/reference/config.md)
- [`docs/reference/providers.md`](docs/reference/providers.md)
- [`docs/reference/channels.md`](docs/reference/channels.md)
- [`docs/operations/runbook.md`](docs/operations/runbook.md)
- [`docs/start/troubleshooting.md`](docs/start/troubleshooting.md)

The docs system is English-only. Please do not add navigation that promises
translations the repository does not ship.

## License

By contributing, you agree that your contributions are licensed under the
[GNU Affero General Public License v3.0](LICENSE).
