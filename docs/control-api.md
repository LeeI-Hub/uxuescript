# 远程控制接口（control API）

本 fork 在 Tauri 后端加了一层本地 HTTP + JSON 接口，用来在**不操作图形界面**的前提下驱动客户端。
典型用途：外部脚本/AI 助手查询进度、启动或停止课程自动化、跳转任务页、直接调用答题内核。

实现位置：`src-tauri/src/control.rs`；接入点：`lib.rs` 的 `setup` 中调用 `control::start()`，
`commands/mask.rs` 的确认框、`commands/chaoxing.rs` 的状态事件会读写 `ControlState`。

## 配置

首次启动会在数据目录生成 `remote.toml`（Windows 默认
`%APPDATA%\top.unraous.uxs\remote.toml`）：

```toml
enabled = true
addr = "127.0.0.1:8765"
token = "<32 位十六进制随机令牌>"
```

- `enabled`：设为 `false` 即完全关闭接口。
- `addr`：默认仅回环地址；要局域网访问改成 `0.0.0.0:8765`（务必配合令牌与防火墙）。
- `token`：所有请求都要带 `Authorization: Bearer <token>`，缺失或错误返回 401。

修改后重启应用生效。启动日志里会打印 `远程控制接口已启动: http://...`。

## 接口

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 存活检查 |
| GET | `/status` | 课程是否运行中、当前 URL、确认策略、最近一次状态事件 |
| POST | `/start` | 启动课程自动化；可选 `{"url": "..."}` 先跳转 |
| POST | `/stop` | 停止课程自动化并重载页面 |
| POST | `/reload` | 重载 chaoxing 当前页面 |
| POST | `/navigate` | `{"url": "..."}` 跳转 |
| POST | `/eval` | `{"js": "..."}` 在 chaoxing 页面执行脚本 |
| POST | `/solve` | `{"payload": <Folio>, "allowPartial": false}` 调答题内核 |
| GET | `/logs?lines=100` | 最新日志尾部 |

`/start` 与 `/stop` 的机制：把页面重载，让注入脚本重新跑；同时把确认框策略临时设为
"自动确认/自动取消"（15 秒后交还界面）。所以远程启动不需要有人点 Yes。

`payload` 与注入脚本 `solve_questions` 的 `Folio` 结构一致，字段见
`src/services/cmds.ts` 中的 `Folio_Deserialize`。

## 示例

```bash
TOKEN=$(grep '^token' "%APPDATA%\top.unraous.uxs\remote.toml" | cut -d'"' -f2)

curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/status
curl -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/start
curl -X POST -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"url":"https://mooc1.chaoxing.com/mycourse/studentstudy?..."}' \
     http://127.0.0.1:8765/start
curl -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/stop
```

## 从外网访问

默认只监听回环地址，外部无法直连。推荐两种方式（任选其一，都必须保留令牌校验）：

1. **Tailscale**：装好后 `tailscale up`，把 `addr` 改成 `0.0.0.0:8765`，
   用 `http://<tailscale-ip>:8765` 访问；或 `tailscale funnel 8765` 暴露到公网。
2. **Cloudflare Tunnel**：`cloudflared tunnel --url http://127.0.0.1:8765`，用它给的域名访问。

安全提醒：这是能操作学习通页面的高权限接口。**不要**把 `addr` 设成 `0.0.0.0` 后
直接暴露到公网而不加令牌/隧道；令牌一旦泄露，等于把客户端交给别人。
