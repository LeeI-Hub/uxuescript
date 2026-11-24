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
| POST | `/discuss` | 自动完成一个“思考讨论”任务点（见下） |
| POST | `/solve` | `{"payload": <Folio>, "allowPartial": false}` 调答题内核 |
| GET | `/logs?lines=100` | 最新日志尾部 |

### `/discuss`：讨论任务点自动回帖

上游脚本只认视频/测验，遇到“思考讨论”会打印 `未识别的任务点类型 insertbbs` 并跳过。
这条链路无法用课程页的发帖接口（`/mooc-ans/bbscircle/grouptopic/publish` 返回 `error, code=4`），
必须走 `groupweb.chaoxing.com` 的话题页回帖，所以单独做成本地接口：

1. 打开 `studentstudy?chapterId=...` 章节页；
2. 在嵌套 iframe（`#iframe > iframe > #frame_content`）里读 `#topicMainDiv` 的 `data` 属性拿到话题地址；
3. 跳转话题页，填 `textarea[placeholder='回复话题']` 并点 `.addReply`。

```bash
curl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"courseId":"265759812","clazzId":"151527283","cpi":"496242403",
       "enc":"<章节页隐藏 enc>","chapterId":"1207300142",
       "text":"回帖内容","dryRun":false}' \
  http://127.0.0.1:8765/discuss
```

- `dryRun: true` 只取话题地址与题干并返回，不提交（用于核对任务点类型）。
- 返回 `{ok, chapterId, question, topicUrl, filled, clicked, confirmed}`；
  `confirmed` 表示话题页已出现本账号新楼层。
- 页面侧结果通过 `POST /report?nonce=<一次性口令>` 经 `sendBeacon` 回传（该路径只认口令、不认令牌，
  因为 beacon 无法带自定义头；口令每次调用重新生成，用完即废）。
- 章节 `enc` 与 `chapterId` 仍需从课程章节页取（`div.posCatalog_select` 的 `onclick="toOld(...)"`
  与页面隐藏 `#enc`），未内置爬取。

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

## 构建与运行踩坑（务必核对）

1. **必须用官方打包链路构建**：`pnpm tauri build --no-bundle`。
   直接 `cargo build --release` 不会执行 `beforeBuildCommand`，前端资源（`dist/`）可能是旧的/不全，
   结果是主界面 WebView 根本不加载（启动日志里没有 `成功获取 AI Provider 列表`、`成功获取配置信息`、
   也没有 `asset not found: favicon.ico` 请求），窗口不会被 show，紧接着**所有 media 元素恒为
   `readyState=0`**（真实课程视频、data:URL 视频、WAV 都不解码），脚本流程也会在配置加载后静默停住。
   产物大小参考：约 13,060,000 字节；若只有 10MB 左右说明前端资源没打进去。
2. **确认框必须走 `chaoxing-mask` 的自动裁决**：`commands/mask.rs` 的 `confirm` 只允许 `main` / `chaoxing`
   两个 WebView 发起，课程页走 `chaoxing-mask` 分支。这里必须插入：
   ```rust
   if mask_label == "chaoxing-mask" {
       if let Some(choice) = window.app_handle()
           .state::<crate::control::ControlState>()
           .auto_confirm()
       {
           log::info!("远程控制自动确认课程确认框: {choice}");
           return Ok(choice);
       }
   }
   ```
   该逻辑一旦丢失（例如 `git checkout src-tauri/src` 把 `mask.rs` 还原成上游版本），`/start` 之后
   确认框会在后台等待一个永远不存在的点击，脚本挂死：日志停在 `配置设置成功` 之后，`/status` 的
   `courseActive` 恒为 `false`，从页面 `invoke("confirm", ...)` 探针会立刻返回 `false`（`try_lock` 失败，
   说明上一次确认还挂着）。**重编前先确认 `mask.rs` 里还有这段**。
3. **`/start` 会重载页面**：重载瞬间 WebView2 会打印 `IPC custom protocol failed... Couldn't find callback id`，
   属正常噪声；但如果之后 30 秒内没有出现 `获取课程列表成功，共 N 个章节`，就是流程没起来，需重启客户端再 `/start`。
4. **补刷用课程树判定，不要靠日志**：注入了 `SELECTORS.courseTree`（`div.posCatalog_select` +
   `span.posCatalog_name`，未完成标记 `:has(.orangeNew)`、未完成数量 `input.jobUnfinishCount`）。
   核对时在 chaoxing 页面执行：
   ```js
   { total: document.querySelectorAll("div.posCatalog_select").length,
     unfinished: document.querySelectorAll("div.posCatalog_select:has(.orangeNew)").length }
   ```
   `unfinished > 0` 就重新 `/start`（脚本普通模式只处理未完成章节，会跳过已完成的）。
5. **观看时长任务点会掉回 1 倍速**：`injected/chaoxing/course/chapter/tasks.js` 检测到任务点带“观看时长”
   条件时执行 `config.lockSpeed = null` 并打印 `有观看时长要求，关闭后续倍速锁定`。这是上游防判无效的
   设计，不要手动调回 2x（上报时长不足容易被判学习时长不足）。
6. **长时间无人值守**：客户端在跑时用 `SetThreadExecutionState(ES_CONTINUOUS|ES_SYSTEM_REQUIRED)`
   的守护进程可以只防睡眠、不拦熄屏；客户端退出后让它自行结束（见 `scripts/keepawake.ps1` 或临时脚本）。

## 从外网访问

默认只监听回环地址，外部无法直连。推荐两种方式（任选其一，都必须保留令牌校验）：

1. **Tailscale**：装好后 `tailscale up`，把 `addr` 改成 `0.0.0.0:8765`，
   用 `http://<tailscale-ip>:8765` 访问；或 `tailscale funnel 8765` 暴露到公网。
2. **Cloudflare Tunnel**：`cloudflared tunnel --url http://127.0.0.1:8765`，用它给的域名访问。

安全提醒：这是能操作学习通页面的高权限接口。**不要**把 `addr` 设成 `0.0.0.0` 后
直接暴露到公网而不加令牌/隧道；令牌一旦泄露，等于把客户端交给别人。
