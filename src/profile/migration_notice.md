# RantaiClaw — Storage layout migrated

Your RantaiClaw config and data have been moved into a profile-aware
layout at `~/.rantaiclaw/profiles/default/`. This is part of v0.5.0
multi-profile support (see `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`
for the full design).

Symlinks at the old paths (`~/.rantaiclaw/config.toml`,
`~/.rantaiclaw/workspace`) are preserved through v0.6.0 and removed in
v0.7.0. Update any external scripts that read those paths to point at
`~/.rantaiclaw/profiles/<profile>/...` instead.

To create additional profiles:

    rantaiclaw profile create work --clone default

Run `rantaiclaw doctor` to verify everything is healthy.
