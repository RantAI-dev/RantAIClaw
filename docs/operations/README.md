# Operations & Deployment Docs

For operators running RantaiClaw in persistent or production-like environments.

## Core Operations

- Day-2 runbook: [runbook.md](runbook.md)
- Release runbook: [../contributing/release-process.md](../contributing/release-process.md)
- Troubleshooting matrix: [../start/troubleshooting.md](../start/troubleshooting.md)
- Safe network/gateway deployment: [network-deployment.md](network-deployment.md)
- Mattermost setup (channel-specific): [../reference/mattermost-setup.md](../reference/mattermost-setup.md)

## Common Flow

1. Validate runtime (`status`, `doctor`, `channel doctor`)
2. Apply one config change at a time
3. Restart service/daemon
4. Verify channel and gateway health
5. Roll back quickly if behavior regresses

## Related

- Config reference: [../reference/config.md](../reference/config.md)
- Security collection: [../security/README.md](../security/README.md)
