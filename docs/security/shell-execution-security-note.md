# Shell execution security note

**Status:** Current behavior (not a proposal)
**Date:** 2026-10-14
**Scope:** `src/tools/shell.rs`, `src/security/policy.rs`, `src/agent/loop_.rs`, `src/runtime/`

Every control below names the code that implements it, at the commit this note was
rewritten against. Line numbers move; re-verify them when the code moves.

---

## 1. Surface

The `shell` tool lets the agent run an arbitrary command string through a shell,
with the configured workspace as the working directory
(`src/runtime/native.rs:62-63`). It is the most powerful tool the agent holds: one
command can rewrite files, install software, open outbound connections or take on
another user's privileges.

Nothing rewrites or escapes the string. On the native runtime it reaches
`/bin/sh -c` (`src/runtime/native.rs:57-63`); on the docker runtime it reaches
`sh -c` inside the container (`src/runtime/docker.rs:111-115`).

---

## 2. Defenses in place

### 2.1 Autonomy policy (`AutonomyLevel`)

`AutonomyLevel` has three values, `ReadOnly`, `Supervised` and `Full`, and
`Supervised` is the default (`src/security/policy.rs:12-23`, mirrored in the
config schema at `src/config/schema.rs:2020-2022`).

| Level | Shell behavior |
|---|---|
| `ReadOnly` | Every command is refused |
| `Supervised` | Runs after `validate_command_execution`; medium and high risk need a human approval |
| `Full` | Runs after `validate_command_execution`; the approval gate does not apply |

The level is read before the command is spawned, so there is no path that skips
it.

Beyond `shell`, the level applies to every tool. Under `ReadOnly` a tool call runs
only when the tool declares that call read-only. The agent loop refuses every
other call before the tool body starts, so MCP tools and skill tools do not run
under `ReadOnly`. A new built-in tool is refused under `ReadOnly` until it
overrides `is_read_only_call` to return `true` for its read-only calls. The
refusal is written to the audit trail with risk level `read_only`. The gate is in
`execute_one_tool_structured` (`src/agent/loop_.rs:1241`), and the check itself at
`src/agent/loop_.rs:1305-1349`. This is one gate for every surface (#1032).

### 2.2 Command validation (`validate_command_execution`)

`validate_command_execution` (`src/security/policy.rs:858-896`) runs two checks in
a fixed order.

The credential-directory refusal runs first, as a hard block
(`src/security/policy.rs:863-865`). It is checked before the allowlist, so it holds
at every autonomy level, `full` included, and no approval lifts it.

The allowlist and risk gate follow (`src/security/policy.rs:867-893`):

- `ReadOnly` refuses everything (`src/security/policy.rs:1068-1070`).
- `Full` skips the allowlist and the structural blocks, and accepts any non-empty
  string (`src/security/policy.rs:1074-1076`).
- `Supervised` refuses subshell and expansion operators (`` ` ``, `$(`, `${`,
  `<(`, `>(`), unquoted output redirects, `tee`, and a lone `&`; it splits on the
  unquoted separators and validates every segment against the allowlist, and it
  blocks dangerous arguments such as `find -exec` and `git config`
  (`src/security/policy.rs:1078-1147`). It then classifies the risk
  (`command_risk_level`, `src/security/policy.rs:702`) and asks for
  `approved: true` on a medium or high risk command
  (`src/security/policy.rs:873-893`).

`approved` never comes from the tool arguments. The model writes its own
arguments, so a model-supplied `approved=true` would let the model wave its own
gate. Approval comes only from the human backend, which denies by default off an
interactive terminal (`src/tools/shell.rs:292-297`).

#### Credential directories

`CREDENTIAL_DIRECTORIES` (`src/security/policy.rs:227-236`) lists `.ssh`, `.aws`,
`.gnupg`, `.config/gh`, `.kube`, `.netrc`, `.azure` and `.config/gcloud`,
relative to the home directory. A command naming one of them is refused at every
autonomy level. The list is not configurable: `[autonomy].forbidden_paths` can add
to the file-tool floor, it can never remove an entry from this one. The refusal
message says so, and `src/tools/shell.rs:316-326` repeats the check before the
approval loop so an approver is never prompted for a block nothing can lift.

The check covers redirect-shaped access too. `word_forms`
(`src/security/policy.rs:318-325`) runs each word through `redirect_targets`
(`src/security/policy.rs:334-350`), which peels the residual after every `<`,
`>`, `>>`, `>|`, `<>` or `&>` run. A word like `>~/.ssh/x` therefore yields both
`>~/.ssh/x` and `~/.ssh/x`, and the second is refused. Quoting is stripped before
this runs, so `echo ">~/.ssh/x"` is refused too, which is the safe direction. The
test `shell_redirect_into_credential_directory_is_refused_and_writes_nothing`
(`src/tools/shell.rs:1174`) pins it: the call is refused under `full` and the child
writes nothing. One list shared by the shell, `pty`, the skill tool and cron came
from #1029; the redirect coverage came from #1035.

### 2.3 Rate limiting

`ActionTracker` (`src/security/policy.rs:42-79`) keeps timestamps in a one-hour
sliding window. On a host whose uptime is under an hour the window keeps every
entry rather than emptying itself, so the limit does not disable itself on a fresh
boot.

The shell tool consults it twice: `is_rate_limited()` before command validation
(`src/tools/shell.rs:299-307`), and `record_action()` after validation, immediately
before the command is spawned (`src/tools/shell.rs:459-467`). The budget is
`[autonomy].max_actions_per_hour` (`src/security/policy.rs:1371-1378`, default 200
at `src/config/schema.rs:2099`). A budget of `0` refuses every call, because
`is_rate_limited()` compares the window count against the budget.

### 2.4 Environment isolation (`env_clear` + `SAFE_ENV_VARS`)

The child starts with an empty environment: `cmd.env_clear()`
(`src/tools/shell.rs:506`). The tool then restores only the variables in
`SAFE_ENV_VARS` (`src/tools/shell.rs:508-512`, list at
`src/tools/shell.rs:37-68`):

- shell essentials: `PATH`, `HOME`, `TERM`, `LANG`, `LC_ALL`, `LC_CTYPE`, `USER`,
  `SHELL`, `TMPDIR`
- proxy configuration: `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` and
  their lowercase forms
- tool pointers: `KUBECONFIG`, `DOCKER_HOST`, `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`,
  `AWS_PROFILE`, `AWS_REGION`, `AWS_DEFAULT_REGION`,
  `GOOGLE_APPLICATION_CREDENTIALS`, `XDG_RUNTIME_DIR`, `KRB5CCNAME`

Most entries are a path, a socket or a region and profile selector, so common
operator tooling works without forwarding a credential. A secret value such as
`AWS_SECRET_ACCESS_KEY` is not on the list and does not reach the child.
`shell_does_not_leak_api_key` (`src/tools/shell.rs:926`) and
`shell_safe_env_vars_excludes_secrets` (`src/tools/shell.rs:1260`) guard this.

One limit is worth stating: the guard checks variable names, not values. A
`*_PROXY` URL may embed `user:pass@` basic auth, so a command that prints the
environment (`env`) can surface it. That is the accepted cost of supporting an
authenticated corporate proxy (`src/tools/shell.rs:23-36`).

### 2.5 Timeout (600 seconds)

`SHELL_TIMEOUT_SECS = 600` (`src/tools/shell.rs:11-14`), applied at
`src/tools/shell.rs:608`. Ten minutes is long enough for a real install (apt,
`docker pull`, an image build) and still bounds a hung command.

The timeout kills the whole process group, not just the direct child. The native
runtime starts each command with `setsid` (`src/runtime/native.rs:69-80`) and a
`ProcGroupKill` guard signals the group, SIGTERM first and SIGKILL after a
two-second grace (`src/tools/shell.rs:127-182`). The same guard fires when the
agent cancels the call. A docker container runs under `dockerd`, outside the
`docker run` CLI's process group, so a `DockerContainerKill` guard runs
`docker rm -f <name>` after the same grace (`src/tools/shell.rs:191-223`).

### 2.6 Output truncation (1 MiB per stream)

`MAX_OUTPUT_BYTES = 1_048_576` (`src/tools/shell.rs:21-22`). stdout and stderr are
each cut at 1 MiB on a character boundary and get a marker line,
`... [output truncated at 1MB]` and `... [stderr truncated at 1MB]`
(`src/tools/shell.rs:617-624`). A command that produces gigabytes cannot grow the
agent's heap without bound. On success, stderr is folded into `output` so the
agent still sees it.

---

## 3. Residual risks and known gaps

### 3.1 No OS-level sandbox

**Risk level:** High

There is no OS-level sandbox. A `Sandbox` trait with Docker, Firejail, Bubblewrap
and Landlock backends used to live in the security module. It had no production
caller, its config key configured nothing, and it was deleted; `src/security/mod.rs:10-19`
is the authoritative statement of what replaced it, which is nothing.

What confines commands today is `[runtime].kind`, `native` or `docker`
(`src/runtime/mod.rs:12-23`; config schema `src/config/schema.rs:2112-2127`,
default `native` at `src/config/schema.rs:2161-2163`). On the default `native`
runtime a command runs as the agent's own user, with that user's rights: a command
the policy allows can read anything that user can read outside the credential
directories, and can write anywhere that user can write. The workspace is the
working directory, not a boundary.

**Available containment:** set `[runtime].kind = "docker"`. The container runs
`--rm --init --interactive` with signal proxying
(`src/runtime/docker.rs:67-73`), gets a memory limit and a CPU limit
(`src/runtime/docker.rs:84-90`), a read-only root filesystem by default
(`src/runtime/docker.rs:92-94`, `[runtime.docker].read_only_rootfs` defaulting to
`true` at `src/config/schema.rs:2188`), and one volume: the workspace at
`/workspace`, with the mount path checked against the allowed workspace roots and
`/` refused (`src/runtime/docker.rs:96-108`,
`src/runtime/docker.rs:17-42`). Container hardening only applies to a docker
runtime; nothing here touches a `native` run.

### 3.2 No network command filtering

**Risk level:** Medium

The policy does not filter network commands. `curl`, `wget`, `nc`, `ssh` and
similar tools run whenever the allowlist admits the command and the binary is on
`PATH`. An approved command can open outbound connections or exfiltrate data.

The setting that removes this exposure is `[runtime.docker].network`, which
defaults to `none` (`src/config/schema.rs:2136-2138` and
`src/config/schema.rs:2169-2171`, applied at `src/runtime/docker.rs:79-83`). It is a
runtime setting, not a policy one.

**Operator recommendation:** enforce egress on the host (firewall, iptables rules),
or keep the docker runtime on `network = "none"`.

### 3.3 Inherited parent-process resources

**Risk level:** Low

The child inherits the descriptors the agent process holds open unless they carry
`O_CLOEXEC`. Neither runtime nor the shell tool closes or enumerates them before
spawning (`src/runtime/native.rs:44-86`, `src/runtime/docker.rs:60-137`,
`src/tools/shell.rs:486-520`), so a socket or file the agent process opened stays
visible to the command.

**Status:** accepted risk. There is no in-repo mitigation; what is exposed depends
on which descriptors the agent process happens to hold. The docker runtime narrows
it, since only the workspace is mounted into the container.

### 3.4 Injection through command content

**Risk level:** Low to medium

`command` reaches a shell verbatim (`src/runtime/native.rs:62-63`,
`src/runtime/docker.rs:111-115`). An agent that builds a command by interpolating
untrusted data can be talked into running something else.

**Defenses in place, under `supervised` only:** `is_command_allowed` refuses
subshell and expansion operators, unquoted redirects, `tee` and a lone `&`, then
validates every segment after splitting on the unquoted separators
(`src/security/policy.rs:1078-1147`), and the risk gate asks a human for medium
and high risk (`src/security/policy.rs:873-893`). Under `full`,
`is_command_allowed` accepts any non-empty string
(`src/security/policy.rs:1074-1076`), so those structural blocks do not apply. The
credential-directory refusal runs at every level
(`src/security/policy.rs:863-865`).

**Recommendation:** configure the model so it does not interpolate untrusted data
into a command string.

---

## 4. Test coverage matrix

Shell tool, `src/tools/shell.rs`:

| Scenario | Test |
|---|---|
| Allowed command runs | `shell_executes_allowed_command` (`:729`) |
| Command outside the allowlist is refused | `shell_blocks_disallowed_command` (`:741`) |
| `ReadOnly` refuses everything | `shell_blocks_readonly` (`:759`) |
| Missing `command` parameter | `shell_missing_command_param` (`:774`) |
| Wrong type for `command` | `shell_wrong_type_param` (`:782`) |
| Non-zero exit code captured | `shell_captures_exit_code` (`:789`) |
| API key not passed to the shell | `shell_does_not_leak_api_key` (`:926`) |
| `PATH` and `HOME` preserved | `shell_preserves_path_and_home` (`:972`) |
| Medium risk needs a human approval, not a model flag | `shell_medium_risk_needs_human_approval_not_a_model_flag` (`:997`) |
| A high-risk block cannot be approved away | `shell_high_risk_block_is_not_approvable` (`:1052`) |
| Credential directory refused with no prompt | `shell_credential_directory_is_refused_without_a_prompt` (`:1086`) |
| Credential directory refused under `full` | `shell_credential_directory_is_refused_under_full_autonomy` (`:1147`) |
| Redirect into a credential directory refused, nothing written | `shell_redirect_into_credential_directory_is_refused_and_writes_nothing` (`:1174`) |
| Timeout constant is 600 s | `shell_timeout_constant_is_reasonable` (`:1242`) |
| Output limit is 1 MiB | `shell_output_limit_is_1mb` (`:1250`) |
| `SAFE_ENV_VARS` excludes secrets | `shell_safe_env_vars_excludes_secrets` (`:1260`) |
| `SAFE_ENV_VARS` includes the essentials | `shell_safe_env_vars_includes_essentials` (`:1271`) |
| `SAFE_ENV_VARS` includes common tooling | `shell_safe_env_vars_includes_common_tooling` (`:1287`) |
| Rate limit refuses execution | `shell_blocks_rate_limited` (`:1308`) |
| Cancel reaps the whole process group | `shell_cancel_reaps_the_whole_process_group` (`:852`) |

Agent loop, `src/agent/loop_.rs`:

| Scenario | Test |
|---|---|
| `ReadOnly` runs the read-only built-ins | `readonly_still_runs_the_read_only_builtin_tools` (`:4238`) |
| `ReadOnly` refuses a skill tool | `readonly_refuses_a_skill_tool_through_the_loop` (`:4281`) |
| `ReadOnly` refuses an MCP tool | `readonly_refuses_an_mcp_tool_through_the_loop` (`:4335`) |
| `ReadOnly` refuses a call on the parallel branch | `readonly_refuses_calls_on_the_parallel_branch` (`:4496`) |
| A `ReadOnly` refusal is audited as refused | `a_readonly_refusal_is_audited_as_refused_not_executed` (`:4543`) |
| The gate does nothing under `supervised` or `full` | `the_readonly_gate_does_nothing_under_supervised_or_full` (`:4702`) |
| An undeclared tool is refused without running | `readonly_refuses_an_undeclared_tool_without_running_it` (`:4777`) |

Not covered by a test: the residual risks in section 3. No test asserts that a
command cannot read outside the workspace, because nothing implements that.

---

## 5. Operator recommendations

```toml
[autonomy]
# supervised is the default; set it explicitly
level = "supervised"

# Action budget per hour; 0 refuses every tool call
max_actions_per_hour = 200

[runtime]
# "docker" is the only OS-level containment that exists today
kind = "docker"

[runtime.docker]
# "none" removes the network exposure the policy does not filter
network = "none"
```

- Credential directories are refused at every autonomy level, including `full`, and
  no approval lifts that. Nothing in config changes it.
- The workspace directory is not a `config.toml` key. It resolves from
  `RANTAICLAW_WORKSPACE`, then the `active_workspace.toml` marker, then the default
  under the config directory (`src/config/schema.rs:59`). Point it at a directory
  holding nothing an approved command should reach.
- There is no sandbox layer to verify at startup, and no kernel log line to grep
  for. If you run `kind = "docker"`, confirm it by running a command that needs
  the container, not by checking a log.
- Network egress is not filtered by the policy (3.2). Use host egress rules or
  `network = "none"`.

---

## 6. Rollback

This document describes current behavior. When `shell.rs`, `SecurityPolicy` or the
runtime changes, update this note in the same pull request.
