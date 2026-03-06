# Sổ tay Vận hành RantaiClaw

Tài liệu này dành cho các operator chịu trách nhiệm duy trì tính sẵn sàng, tình trạng bảo mật và xử lý sự cố.

Cập nhật lần cuối: **2026-02-18**.

## Phạm vi

Dùng tài liệu này cho các tác vụ vận hành day-2:

- khởi động và giám sát runtime
- kiểm tra sức khoẻ và chẩn đoán hệ thống
- triển khai an toàn và rollback
- phân loại và khôi phục sau sự cố

Nếu đây là lần cài đặt đầu tiên, hãy bắt đầu từ [one-click-bootstrap.md](one-click-bootstrap.md).

## Các chế độ Runtime

| Chế độ | Lệnh | Khi nào dùng |
|---|---|---|
| Foreground runtime | `rantaiclaw daemon` | gỡ lỗi cục bộ, phiên ngắn |
| Foreground gateway only | `rantaiclaw gateway` | kiểm thử webhook endpoint |
| User service | `rantaiclaw service install && rantaiclaw service start` | runtime được quản lý liên tục bởi operator |

## Checklist Cơ bản cho Operator

1. Xác thực cấu hình:

```bash
rantaiclaw status
```

2. Kiểm tra chẩn đoán:

```bash
rantaiclaw doctor
rantaiclaw channel doctor
```

3. Khởi động runtime:

```bash
rantaiclaw daemon
```

4. Để chạy như user session service liên tục:

```bash
rantaiclaw service install
rantaiclaw service start
rantaiclaw service status
```

## Tín hiệu Sức khoẻ và Trạng thái

| Tín hiệu | Lệnh / File | Kỳ vọng |
|---|---|---|
| Tính hợp lệ của config | `rantaiclaw doctor` | không có lỗi nghiêm trọng |
| Kết nối channel | `rantaiclaw channel doctor` | các channel đã cấu hình đều khoẻ mạnh |
| Tóm tắt runtime | `rantaiclaw status` | provider/model/channels như mong đợi |
| Heartbeat/trạng thái daemon | `~/.rantaiclaw/daemon_state.json` | file được cập nhật định kỳ |

## Log và Chẩn đoán

### macOS / Windows (log của service wrapper)

- `~/.rantaiclaw/logs/daemon.stdout.log`
- `~/.rantaiclaw/logs/daemon.stderr.log`

### Linux (systemd user service)

```bash
journalctl --user -u rantaiclaw.service -f
```

## Quy trình Phân loại Sự cố (Fast Path)

1. Chụp trạng thái hệ thống:

```bash
rantaiclaw status
rantaiclaw doctor
rantaiclaw channel doctor
```

2. Kiểm tra trạng thái service:

```bash
rantaiclaw service status
```

3. Nếu service không khoẻ, khởi động lại sạch:

```bash
rantaiclaw service stop
rantaiclaw service start
```

4. Nếu các channel vẫn thất bại, kiểm tra allowlist và thông tin xác thực trong `~/.rantaiclaw/config.toml`.

5. Nếu liên quan đến gateway, kiểm tra cài đặt bind/auth (`[gateway]`) và khả năng tiếp cận cục bộ.

## Quy trình Thay đổi An toàn

Trước khi áp dụng thay đổi cấu hình:

1. sao lưu `~/.rantaiclaw/config.toml`
2. chỉ áp dụng một thay đổi logic tại một thời điểm
3. chạy `rantaiclaw doctor`
4. khởi động lại daemon/service
5. xác minh bằng `status` + `channel doctor`

## Quy trình Rollback

Nếu một lần triển khai gây ra suy giảm hành vi:

1. khôi phục `config.toml` trước đó
2. khởi động lại runtime (`daemon` hoặc `service`)
3. xác nhận khôi phục qua `doctor` và kiểm tra sức khoẻ channel
4. ghi lại nguyên nhân gốc rễ và biện pháp khắc phục sự cố

## Tài liệu Liên quan

- [one-click-bootstrap.md](one-click-bootstrap.md)
- [troubleshooting.md](troubleshooting.md)
- [config-reference.md](config-reference.md)
- [commands-reference.md](commands-reference.md)
