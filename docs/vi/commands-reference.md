# Tham khảo lệnh RantaiClaw

Dựa trên CLI hiện tại (`rantaiclaw --help`).

Xác minh lần cuối: **2026-02-20**.

## Lệnh cấp cao nhất

| Lệnh | Mục đích |
|---|---|
| `onboard` | Khởi tạo workspace/config nhanh hoặc tương tác |
| `agent` | Chạy chat tương tác hoặc chế độ gửi tin nhắn đơn |
| `gateway` | Khởi động gateway webhook và HTTP WhatsApp |
| `daemon` | Khởi động runtime có giám sát (gateway + channels + heartbeat/scheduler tùy chọn) |
| `service` | Quản lý vòng đời dịch vụ cấp hệ điều hành |
| `doctor` | Chạy chẩn đoán và kiểm tra trạng thái |
| `status` | Hiển thị cấu hình và tóm tắt hệ thống |
| `cron` | Quản lý tác vụ định kỳ |
| `models` | Làm mới danh mục model của provider |
| `providers` | Liệt kê ID provider, bí danh và provider đang dùng |
| `channel` | Quản lý kênh và kiểm tra sức khỏe kênh |
| `integrations` | Kiểm tra chi tiết tích hợp |
| `skills` | Liệt kê/cài đặt/gỡ bỏ skills |
| `migrate` | Nhập dữ liệu từ runtime khác (hiện hỗ trợ OpenClaw) |
| `config` | Xuất schema cấu hình dạng máy đọc được |
| `completions` | Tạo script tự hoàn thành cho shell ra stdout |
| `hardware` | Phát hiện và kiểm tra phần cứng USB |
| `peripheral` | Cấu hình và nạp firmware thiết bị ngoại vi |

## Nhóm lệnh

### `onboard`

- `rantaiclaw onboard`
- `rantaiclaw onboard --interactive`
- `rantaiclaw onboard --channels-only`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --memory <sqlite|lucid|markdown|none>`
- `rantaiclaw onboard --api-key <KEY> --provider <ID> --model <MODEL_ID> --memory <sqlite|lucid|markdown|none>`

### `agent`

- `rantaiclaw agent`
- `rantaiclaw agent -m "Hello"`
- `rantaiclaw agent --provider <ID> --model <MODEL> --temperature <0.0-2.0>`
- `rantaiclaw agent --peripheral <board:path>`

### `gateway` / `daemon`

- `rantaiclaw gateway [--host <HOST>] [--port <PORT>]`
- `rantaiclaw daemon [--host <HOST>] [--port <PORT>]`

### `service`

- `rantaiclaw service install`
- `rantaiclaw service start`
- `rantaiclaw service stop`
- `rantaiclaw service restart`
- `rantaiclaw service status`
- `rantaiclaw service uninstall`

### `cron`

- `rantaiclaw cron list`
- `rantaiclaw cron add <expr> [--tz <IANA_TZ>] <command>`
- `rantaiclaw cron add-at <rfc3339_timestamp> <command>`
- `rantaiclaw cron add-every <every_ms> <command>`
- `rantaiclaw cron once <delay> <command>`
- `rantaiclaw cron remove <id>`
- `rantaiclaw cron pause <id>`
- `rantaiclaw cron resume <id>`

### `models`

- `rantaiclaw models refresh`
- `rantaiclaw models refresh --provider <ID>`
- `rantaiclaw models refresh --force`

`models refresh` hiện hỗ trợ làm mới danh mục trực tiếp cho các provider: `openrouter`, `openai`, `anthropic`, `groq`, `mistral`, `deepseek`, `xai`, `together-ai`, `gemini`, `ollama`, `astrai`, `venice`, `fireworks`, `cohere`, `moonshot`, `glm`, `zai`, `qwen` và `nvidia`.

### `channel`

- `rantaiclaw channel list`
- `rantaiclaw channel start`
- `rantaiclaw channel doctor`
- `rantaiclaw channel bind-telegram <IDENTITY>`
- `rantaiclaw channel add <type> <json>`
- `rantaiclaw channel remove <name>`

Lệnh trong chat khi runtime đang chạy (Telegram/Discord):

- `/models`
- `/models <provider>`
- `/model`
- `/model <model-id>`

Channel runtime cũng theo dõi `config.toml` và tự động áp dụng thay đổi cho:
- `default_provider`
- `default_model`
- `default_temperature`
- `api_key` / `api_url` (cho provider mặc định)
- `reliability.*` cài đặt retry của provider

`add/remove` hiện chuyển hướng về thiết lập có hướng dẫn / cấu hình thủ công (chưa hỗ trợ đầy đủ mutator khai báo).

### `integrations`

- `rantaiclaw integrations info <name>`

### `skills`

- `rantaiclaw skills list`
- `rantaiclaw skills install <source>`
- `rantaiclaw skills remove <name>`

`<source>` chấp nhận git remote (`https://...`, `http://...`, `ssh://...` và `git@host:owner/repo.git`) hoặc đường dẫn cục bộ.

Skill manifest (`SKILL.toml`) hỗ trợ `prompts` và `[[tools]]`; cả hai được đưa vào system prompt của agent khi chạy, giúp model có thể tuân theo hướng dẫn skill mà không cần đọc thủ công.

### `migrate`

- `rantaiclaw migrate openclaw [--source <path>] [--dry-run]`

### `config`

- `rantaiclaw config schema`

`config schema` xuất JSON Schema (draft 2020-12) cho toàn bộ hợp đồng `config.toml` ra stdout.

### `completions`

- `rantaiclaw completions bash`
- `rantaiclaw completions fish`
- `rantaiclaw completions zsh`
- `rantaiclaw completions powershell`
- `rantaiclaw completions elvish`

`completions` chỉ xuất ra stdout để script có thể được source trực tiếp mà không bị lẫn log/cảnh báo.

### `hardware`

- `rantaiclaw hardware discover`
- `rantaiclaw hardware introspect <path>`
- `rantaiclaw hardware info [--chip <chip_name>]`

### `peripheral`

- `rantaiclaw peripheral list`
- `rantaiclaw peripheral add <board> <path>`
- `rantaiclaw peripheral flash [--port <serial_port>]`
- `rantaiclaw peripheral setup-uno-q [--host <ip_or_host>]`
- `rantaiclaw peripheral flash-nucleo`

## Kiểm tra nhanh

Để xác minh nhanh tài liệu với binary hiện tại:

```bash
rantaiclaw --help
rantaiclaw <command> --help
```
