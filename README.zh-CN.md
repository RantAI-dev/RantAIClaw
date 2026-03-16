<p align="center">
  <img src="rantaiclaw.png" alt="RantaiClaw" width="200" />
</p>

<h1 align="center">RantaiClaw 🦀（简体中文）</h1>

<p align="center">
  <strong>零开销、零妥协；随处部署、万物可换。</strong>
</p>

<p align="center">
  <a href="https://x.com/rantaiclawlabs?s=21"><img src="https://img.shields.io/badge/X-%40rantaiclawlabs-000000?style=flat&logo=x&logoColor=white" alt="X: @rantaiclawlabs" /></a>
  <a href="https://www.xiaohongshu.com/user/profile/67cbfc43000000000d008307?xsec_token=AB73VnYnGNx5y36EtnnZfGmAmS-6Wzv8WMuGpfwfkg6Yc%3D&xsec_source=pc_search"><img src="https://img.shields.io/badge/Xiaohongshu-Official-FF2442?style=flat" alt="Xiaohongshu: Official" /></a>
  <a href="https://t.me/rantaiclawlabs"><img src="https://img.shields.io/badge/Telegram-%40rantaiclawlabs-26A5E4?style=flat&logo=telegram&logoColor=white" alt="Telegram: @rantaiclawlabs" /></a>
  <a href="https://t.me/rantaiclawlabs_cn"><img src="https://img.shields.io/badge/Telegram%20CN-%40rantaiclawlabs__cn-26A5E4?style=flat&logo=telegram&logoColor=white" alt="Telegram CN: @rantaiclawlabs_cn" /></a>
  <a href="https://t.me/rantaiclawlabs_ru"><img src="https://img.shields.io/badge/Telegram%20RU-%40rantaiclawlabs__ru-26A5E4?style=flat&logo=telegram&logoColor=white" alt="Telegram RU: @rantaiclawlabs_ru" /></a>
  <a href="https://www.reddit.com/r/rantaiclawlabs/"><img src="https://img.shields.io/badge/Reddit-r%2Frantaiclawlabs-FF4500?style=flat&logo=reddit&logoColor=white" alt="Reddit: r/rantaiclawlabs" /></a>
</p>

<p align="center">
  🌐 语言：<a href="README.md">English</a> · <a href="README.zh-CN.md">简体中文</a> · <a href="README.ja.md">日本語</a> · <a href="README.ru.md">Русский</a> · <a href="README.fr.md">Français</a> · <a href="README.vi.md">Tiếng Việt</a>
</p>

<p align="center">
  <a href="bootstrap.sh">一键部署</a> |
  <a href="docs/getting-started/README.md">安装入门</a> |
  <a href="docs/README.zh-CN.md">文档总览</a> |
  <a href="docs/SUMMARY.md">文档目录</a>
</p>

<p align="center">
  <strong>场景分流：</strong>
  <a href="docs/reference/README.md">参考手册</a> ·
  <a href="docs/operations/README.md">运维部署</a> ·
  <a href="docs/troubleshooting.md">故障排查</a> ·
  <a href="docs/security/README.md">安全专题</a> ·
  <a href="docs/hardware/README.md">硬件外设</a> ·
  <a href="docs/contributing/README.md">贡献与 CI</a>
</p>

> 本文是对 `README.md` 的人工对齐翻译（强调可读性与准确性，不做逐字直译）。
> 
> 技术标识（命令、配置键、API 路径、Trait 名称）保持英文，避免语义漂移。
> 
> 最后对齐时间：**2026-02-19**。

## 📢 公告板

用于发布重要通知（破坏性变更、安全通告、维护窗口、版本阻塞问题等）。

| 日期（UTC） | 级别 | 通知 | 处理建议 |
|---|---|---|---|
| 2026-02-19 | _紧急_ | 我们与 `openagen/rantaiclaw` 及 `rantaiclaw.org` **没有任何关系**。`rantaiclaw.org` 当前会指向 `openagen/rantaiclaw` 这个 fork，并且该域名/仓库正在冒充我们的官网与官方项目。 | 请不要相信上述来源发布的任何信息、二进制、募资活动或官方声明。请仅以本仓库和已验证官方社媒为准。 |
| 2026-02-19 | _重要_ | 我们目前**尚未发布官方正式网站**，且已发现有人尝试冒充我们。请勿参与任何打着 RantaiClaw 名义进行的投资、募资或类似活动。 | 一切信息请以本仓库为准；也可关注 [X（@rantaiclawlabs）](https://x.com/rantaiclawlabs?s=21)、[Reddit（r/rantaiclawlabs）](https://www.reddit.com/r/rantaiclawlabs/)、[Telegram（@rantaiclawlabs）](https://t.me/rantaiclawlabs)、[Telegram 中文频道（@rantaiclawlabs_cn）](https://t.me/rantaiclawlabs_cn)、[Telegram 俄语频道（@rantaiclawlabs_ru）](https://t.me/rantaiclawlabs_ru) 与 [小红书账号](https://www.xiaohongshu.com/user/profile/67cbfc43000000000d008307?xsec_token=AB73VnYnGNx5y36EtnnZfGmAmS-6Wzv8WMuGpfwfkg6Yc%3D&xsec_source=pc_search) 获取官方最新动态。 |
| 2026-02-19 | _重要_ | Anthropic 于 2026-02-19 更新了 Authentication and Credential Use 条款。条款明确：OAuth authentication（用于 Free、Pro、Max）仅适用于 Claude Code 与 Claude.ai；将 Claude Free/Pro/Max 账号获得的 OAuth token 用于其他任何产品、工具或服务（包括 Agent SDK）不被允许，并可能构成对 Consumer Terms of Service 的违规。 | 为避免损失，请暂时不要尝试 Claude Code OAuth 集成；原文见：[Authentication and Credential Use](https://code.claude.com/docs/en/legal-and-compliance#authentication-and-credential-use)。 |

## 项目简介

RantaiClaw 是一个高性能、低资源占用、可组合的自主智能体运行时：

- Rust 原生实现，单二进制部署，跨 ARM / x86 / RISC-V。
- Trait 驱动架构，`Provider` / `Channel` / `Tool` / `Memory` 可替换。
- 安全默认值优先：配对鉴权、显式 allowlist、沙箱与作用域约束。

## 为什么选择 RantaiClaw

- **默认轻量运行时**：常见 CLI 与 `status` 工作流通常保持在几 MB 级内存范围。
- **低成本部署友好**：面向低价板卡与小规格云主机设计，不依赖厚重运行时。
- **冷启动很快**：Rust 单二进制让常用命令与守护进程启动更接近“秒开”。
- **跨架构可移植**：同一套二进制优先流程覆盖 ARM / x86 / RISC-V，并保持 provider/channel/tool 可替换。

## 基准快照（RantaiClaw vs OpenClaw，可复现）

以下是本地快速基准对比（macOS arm64，2026 年 2 月），按 0.8GHz 边缘 CPU 进行归一化展示：

| | OpenClaw | NanoBot | PicoClaw | RantaiClaw 🦀 |
|---|---|---|---|---|
| **语言** | TypeScript | Python | Go | **Rust** |
| **RAM** | > 1GB | > 100MB | < 10MB | **< 5MB** |
| **启动时间（0.8GHz 核）** | > 500s | > 30s | < 1s | **< 10ms** |
| **二进制体积** | ~28MB（dist） | N/A（脚本） | ~8MB | **~8.8 MB** |
| **成本** | Mac Mini $599 | Linux SBC ~$50 | Linux 板卡 $10 | **任意 $10 硬件** |

> 说明：RantaiClaw 的数据来自 release 构建，并通过 `/usr/bin/time -l` 测得。OpenClaw 需要 Node.js 运行时环境，仅该运行时通常就会带来约 390MB 的额外内存占用；NanoBot 需要 Python 运行时环境。PicoClaw 与 RantaiClaw 为静态二进制。

<p align="center">
  <img src="zero-claw.jpeg" alt="RantaiClaw vs OpenClaw 对比图" width="800" />
</p>

### 本地可复现测量

基准数据会随代码与工具链变化，建议始终在你的目标环境自行复测：

```bash
cargo build --release
ls -lh target/release/rantaiclaw

/usr/bin/time -l target/release/rantaiclaw --help
/usr/bin/time -l target/release/rantaiclaw status
```

当前 README 的样例数据（macOS arm64，2026-02-18）：

- Release 二进制：`8.8M`
- `rantaiclaw --help`：约 `0.02s`，峰值内存约 `3.9MB`
- `rantaiclaw status`：约 `0.01s`，峰值内存约 `4.1MB`

## 一键部署

```bash
git clone https://github.com/rantaiclaw-labs/rantaiclaw.git
cd rantaiclaw
./bootstrap.sh
```

可选环境初始化：`./bootstrap.sh --install-system-deps --install-rust`（可能需要 `sudo`）。

详细说明见：[`docs/one-click-bootstrap.md`](docs/one-click-bootstrap.md)。

## 快速开始

### Homebrew（macOS/Linuxbrew）

```bash
brew install rantaiclaw
```

```bash
git clone https://github.com/rantaiclaw-labs/rantaiclaw.git
cd rantaiclaw
cargo build --release --locked
cargo install --path . --force --locked

# 快速初始化（无交互）
rantaiclaw onboard --api-key sk-... --provider openrouter

# 或使用交互式向导
rantaiclaw onboard --interactive

# 单次对话
rantaiclaw agent -m "Hello, RantaiClaw!"

# 启动网关（默认: 127.0.0.1:3000）
rantaiclaw gateway

# 启动长期运行模式
rantaiclaw daemon
```

## Subscription Auth（OpenAI Codex / Claude Code）

RantaiClaw 现已支持基于订阅的原生鉴权配置（多账号、静态加密存储）。

- 配置文件：`~/.rantaiclaw/auth-profiles.json`
- 加密密钥：`~/.rantaiclaw/.secret_key`
- Profile ID 格式：`<provider>:<profile_name>`（例：`openai-codex:work`）

OpenAI Codex OAuth（ChatGPT 订阅）：

```bash
# 推荐用于服务器/无显示器环境
rantaiclaw auth login --provider openai-codex --device-code

# 浏览器/回调流程，支持粘贴回退
rantaiclaw auth login --provider openai-codex --profile default
rantaiclaw auth paste-redirect --provider openai-codex --profile default

# 检查 / 刷新 / 切换 profile
rantaiclaw auth status
rantaiclaw auth refresh --provider openai-codex --profile default
rantaiclaw auth use --provider openai-codex --profile work
```

Claude Code / Anthropic setup-token：

```bash
# 粘贴订阅/setup token（Authorization header 模式）
rantaiclaw auth paste-token --provider anthropic --profile default --auth-kind authorization

# 别名命令
rantaiclaw auth setup-token --provider anthropic --profile default
```

使用 subscription auth 运行 agent：

```bash
rantaiclaw agent --provider openai-codex -m "hello"
rantaiclaw agent --provider openai-codex --auth-profile openai-codex:work -m "hello"

# Anthropic 同时支持 API key 和 auth token 环境变量：
# ANTHROPIC_AUTH_TOKEN, ANTHROPIC_OAUTH_TOKEN, ANTHROPIC_API_KEY
rantaiclaw agent --provider anthropic -m "hello"
```

## 架构

每个子系统都是一个 **Trait** — 通过配置切换即可更换实现，无需修改代码。

<p align="center">
  <img src="docs/architecture.svg" alt="RantaiClaw 架构图" width="900" />
</p>

| 子系统 | Trait | 内置实现 | 扩展方式 |
|--------|-------|----------|----------|
| **AI 模型** | `Provider` | 通过 `rantaiclaw providers` 查看（当前 28 个内置 + 别名，以及自定义端点） | `custom:https://your-api.com`（OpenAI 兼容）或 `anthropic-custom:https://your-api.com` |
| **通道** | `Channel` | CLI, Telegram, Discord, Slack, Mattermost, iMessage, Matrix, Signal, WhatsApp, Email, IRC, Lark, DingTalk, QQ, Webhook | 任意消息 API |
| **记忆** | `Memory` | SQLite 混合搜索, PostgreSQL 后端, Lucid 桥接, Markdown 文件, 显式 `none` 后端, 快照/恢复, 可选响应缓存 | 任意持久化后端 |
| **工具** | `Tool` | shell/file/memory, cron/schedule, git, pushover, browser, http_request, screenshot/image_info, composio (opt-in), delegate, 硬件工具 | 任意能力 |
| **可观测性** | `Observer` | Noop, Log, Multi | Prometheus, OTel |
| **运行时** | `RuntimeAdapter` | Native, Docker（沙箱） | 通过 adapter 添加；不支持的类型会快速失败 |
| **安全** | `SecurityPolicy` | Gateway 配对, 沙箱, allowlist, 速率限制, 文件系统作用域, 加密密钥 | — |
| **身份** | `IdentityConfig` | OpenClaw (markdown), AIEOS v1.1 (JSON) | 任意身份格式 |
| **隧道** | `Tunnel` | None, Cloudflare, Tailscale, ngrok, Custom | 任意隧道工具 |
| **心跳** | Engine | HEARTBEAT.md 定期任务 | — |
| **技能** | Loader | TOML 清单 + SKILL.md 指令 | 社区技能包 |
| **集成** | Registry | 9 个分类下 70+ 集成 | 插件系统 |

### 运行时支持（当前）

- ✅ 当前支持：`runtime.kind = "native"` 或 `runtime.kind = "docker"`
- 🚧 计划中，尚未实现：WASM / 边缘运行时

配置了不支持的 `runtime.kind` 时，RantaiClaw 会以明确的错误退出，而非静默回退到 native。

### 记忆系统（全栈搜索引擎）

全部自研，零外部依赖 — 无需 Pinecone、Elasticsearch、LangChain：

| 层级 | 实现 |
|------|------|
| **向量数据库** | Embeddings 以 BLOB 存储于 SQLite，余弦相似度搜索 |
| **关键词搜索** | FTS5 虚拟表，BM25 评分 |
| **混合合并** | 自定义加权合并函数（`vector.rs`） |
| **Embeddings** | `EmbeddingProvider` trait — OpenAI、自定义 URL 或 noop |
| **分块** | 基于行的 Markdown 分块器，保留标题结构 |
| **缓存** | SQLite `embedding_cache` 表，LRU 淘汰策略 |
| **安全重索引** | 原子化重建 FTS5 + 重新嵌入缺失向量 |

Agent 通过工具自动进行记忆的回忆、保存和管理。

```toml
[memory]
backend = "sqlite"             # "sqlite", "lucid", "postgres", "markdown", "none"
auto_save = true
embedding_provider = "none"    # "none", "openai", "custom:https://..."
vector_weight = 0.7
keyword_weight = 0.3
```

## 安全默认行为（关键）

- Gateway 默认绑定：`127.0.0.1:3000`
- Gateway 默认要求配对：`require_pairing = true`
- 默认拒绝公网绑定：`allow_public_bind = false`
- Channel allowlist 语义：
  - 空列表 `[]` => deny-by-default
  - `"*"` => allow all（仅在明确知道风险时使用）

## 常用配置片段

```toml
api_key = "sk-..."
default_provider = "openrouter"
default_model = "anthropic/claude-sonnet-4-6"
default_temperature = 0.7

[memory]
backend = "sqlite"             # sqlite | lucid | markdown | none
auto_save = true
embedding_provider = "none"    # none | openai | custom:https://...

[gateway]
host = "127.0.0.1"
port = 3000
require_pairing = true
allow_public_bind = false
```

## 文档导航（推荐从这里开始）

- 文档总览（英文）：[`docs/README.md`](docs/README.md)
- 统一目录（TOC）：[`docs/SUMMARY.md`](docs/SUMMARY.md)
- 文档总览（简体中文）：[`docs/README.zh-CN.md`](docs/README.zh-CN.md)
- 命令参考：[`docs/commands-reference.md`](docs/commands-reference.md)
- 配置参考：[`docs/config-reference.md`](docs/config-reference.md)
- Provider 参考：[`docs/providers-reference.md`](docs/providers-reference.md)
- Channel 参考：[`docs/channels-reference.md`](docs/channels-reference.md)
- 运维手册：[`docs/operations-runbook.md`](docs/operations-runbook.md)
- 故障排查：[`docs/troubleshooting.md`](docs/troubleshooting.md)
- 文档清单与分类：[`docs/docs-inventory.md`](docs/docs-inventory.md)
- 项目 triage 快照（2026-02-18）：[`docs/project-triage-snapshot-2026-02-18.md`](docs/project-triage-snapshot-2026-02-18.md)

## 贡献与许可证

- 贡献指南：[`CONTRIBUTING.md`](CONTRIBUTING.md)
- PR 工作流：[`docs/pr-workflow.md`](docs/pr-workflow.md)
- Reviewer 指南：[`docs/reviewer-playbook.md`](docs/reviewer-playbook.md)
- 许可证：MIT（见 [`LICENSE`](LICENSE) 与 [`NOTICE`](NOTICE)）

---

如果你需要完整实现细节（架构图、全部命令、完整 API、开发流程），请直接阅读英文主文档：[`README.md`](README.md)。
