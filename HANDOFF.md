# Codex 手机端 · 工程成果总结 & 云环境交接

> 交付日期：2026-10-05 · 编写者：小寒（沙箱执行层）
> 用途：把本工程搬到云环境继续跑时的**唯一权威交接文档**

---

## 1. 一句话目标

fork `openai/codex` → 做成**独立运行在 Android 手机上的 coding agent**：
支持 `wire_api = "chat"`（接第三方中转站 chat API，兼容 responses），
**完全脱离 ChatGPT 账号**，不依赖 OpenMinis。

---

## 2. 仓库与状态

| 项 | 值 |
|---|---|
| 仓库 | `https://github.com/rdztfxygcysnb-max/codex`（fork 自 openai/codex） |
| 分支 | `main` |
| 上游基线 | `823ea830c0`（2026-10-05 的 main） |
| 当前 HEAD | `593cc419dd` |
| 领先上游 | **21 个 commit，30 文件，+2892 / -17** |
| 本地工作副本 | `/var/minis/shared/codex-fork`（199.6M，工作树干净） |
| CI 状态 | `mobile-ci` + `build-android` 两个 workflow；**注意并发=1**（同账号同时只跑一个，旧 run 会堵住新 run，排查时主动 cancel） |
| 最新 CI | `mobile-ci` run `37409425948` → **success**；`build-android` run `37409425912` → **success**（12/12 步，46m34s） |

**token 存放**（不在本文档）：`/root/.config/codex-gh/token`（600 权限，GitHub device flow 发放）。
过期重建：`https://github.com/login/device`，client_id `178c6fc778ccc68e1d6a`，scope `public_repo workflow`。
**不写进任何日志/仓库/文档。**

---

## 3. 架构：chat 接入点（数据流）

```
config.toml (wire_api = "chat")
  → model-provider-info::WireApi::Chat           [已恢复，上游已删]
  → core::client.rs::stream() 匹配 Chat 分支
      → ModelClientSession::stream_chat_completions()
          → chat_bridge::chat_history_from_prompt(prompt)   Prompt → HistoryItem
          → chat_bridge::chat_tools_from_prompt(&prompt.tools) ToolSpec → chat ToolSpec
                                                           (freeform apply_patch → function 版)
          → codex_api::ChatClient::stream_chat(...)
              → EndpointSession::stream_encoded_json_with(POST /chat/completions)
              → spawn_chat_stream():
                    bytes → chat_provider::SseParser → chat_wire::StreamAccumulator
                    → emit_event()  (TextDelta 用 OutputItemAdded/Done(Message) 包裹)
                    → ResponseEvent → mpsc → ResponseStream
          → core::map_response_stream() → core::ResponseStream
  → session/turn.rs 消费（active_item 状态机）
```

---

## 4. 成果清单（8 个新文件）

| 文件 | 行数 | 作用 |
|---|---|---|
| `codex-rs/codex-api/src/chat_wire.rs` | 901 | 请求构造（历史→Chat Completions body）、SSE 流累积器、tool-arg JSON 修复（Valid/Repaired/Truncated/**Unrecoverable**）、Quirks 别名 |
| `codex-rs/codex-api/src/chat_provider.rs` | ~700 | provider 配置、HTTP 错误分类（Retryable/RateLimited/Quota/ContextOverflow…）、重试策略、SSE 解析器、超时判断、断流建议 |
| `codex-rs/codex-api/src/endpoint/chat.rs` | 487 | ChatClient：POST + 流映射 + **TextDelta 的 item 包裹** + 3 个单测 |
| `codex-rs/core/src/chat_bridge.rs` | 275 | Prompt→HistoryItem / ToolSpec→chat ToolSpec 转换 + 3 个单测 |
| `.github/workflows/mobile-ci.yml` | ~120 | 每次 push：4 组 cargo check + 4 组单测 + schema 一致性 + **隔离测试** |
| `.github/workflows/build-android.yml` | 67 | NDK 交叉编译 `aarch64-linux-android` → `jniLibs/arm64-v8a/libcodex.so` |
| `scripts/mock_chat_relay.py` | 38 | 隔离测试用的本地 SSE 中转站（127.0.0.1:8899） |
| `scripts/deny_proxy.py` | 40 | HTTPS_PROXY 拒绝代理：打印被访问域名 + 立即 403（消除卡顿） |

**修改的上游文件（19 个）**：`model-provider-info/lib.rs`（WireApi::Chat + ChatQuirks + quirks 字段）、`core/client.rs`（Chat 分发 + stream_chat_completions）、`core/config/mod.rs`（自包含 provider 隔离）、`http-client/Cargo.toml + route_aware_client_pool.rs`（去 openssl）、`config.schema.json`（同步）、若干测试文件（补 `quirks: None` 字段）、`model-provider-info/tests`（chat 反序列化改断言）。

---

## 5. 已完成步骤与**真实验证证据**

### 步骤 1 ✅ — 两文件编译+测试通过
- 沙箱独立 crate：`cargo test` → **18 passed**（反证过：改坏断言变红）
- CI：`cargo check -p codex-api` + 单测 全绿

### 步骤 2 ✅ — wire_api="chat" 接入
| commit | 内容 |
|---|---|
| `7a03262708` | 恢复 WireApi::Chat + ChatQuirks + `[model_providers.<id>.quirks]` |
| `5081a30a7b` | 26 处显式字面量补 `quirks: None`（E0063） |
| `f34cb463b0` | config.schema.json 由 CI 生成器同步 + schema 一致性测试进 CI |
| `00de8c4d1e` | chat_wire 复用 model-provider-info 的 ChatQuirks（单一来源） |
| `071a702081` | ChatClient（endpoint/chat.rs） |
| `e7e1ab97d8` + `0e8f625430` | core 接入（修复了「插到 #[instrument] 属性内」的位置错误 + 走 map_response_stream） |
- **CI run 37396598797 / 37399724155 → success**（含 `cargo check -p codex-core`、4 组单测、schema 一致性）

### 步骤 3 ✅ — 脱离 ChatGPT（代码层自动隔离）
- `95f9d58f18`：`Config::load` 中 **`requires_openai_auth = false` 的 provider → 自动关 update/analytics/feedback**
  （不依赖 config.toml，用户记不住键名也不怕）
- **隔离测试 run `37341047794` → success**，证据链：
  - deny_proxy 日志：**空**（零外部 HTTPS 尝试）
  - strace connect 目标：**全部 loopback** → `OK: only loopback connections during the chat run`
  - codex 完整跑完一轮 "say hi"（exit 0，无 panic）
- **调试过程（方法论）**：tcpdump/DNS 抓取无效 → 改用 **HTTPS_PROXY 拒绝代理**（快 + 打印域名）；
  并在测试**移除**所有关闭项来证明代码层生效

### 中途抓到并修掉的真 bug
- `bd2bbc95ec`：`OutputTextDelta without active item`（debug 构建 panic）→ TextDelta 用 assistant Message 的 Added/Done 包裹（新增单测覆盖）
- `bc731a167f`（产物失败）→ 根因：**cargo 拒绝 member manifest 里 `workspace = true` + `default-features = false`**
  → `593cc419dd` 改到 workspace 根（reqwest 全 rustls、无 default-tls/openssl）

### 步骤 4 ✅ — Android 构建成功
- `c29b6eb631` 加 build-android.yml → 首跑失败：`openssl-sys`（native-tls 拉进来的）
- `bc731a167f` 首轮修：native-tls 移到 `cfg(not(target_os = "android"))` → 二轮失败：cargo 拒绝 member manifest 里 `workspace = true` + `default-features = false`
- `593cc419dd` 二轮修：`default-features = false` 挪到 **workspace 根**（reqwest 全 rustls，无 default-tls/openssl）
- 侦察结论：`linux-sandbox` 有 `cfg(target_os="linux")` 守卫，**Android 自动跳过**（Android 的 `target_os="android"`）
- **run `37409425912` → success**（12/12 步，46m34s），产物 artifact `codex-android-arm64` = **354 MB**
- ⚠️ 产物 `libcodex.so` 未 strip，进 APK 前必须瘦身（`llvm-strip` 或 `strip` + `panic=abort` + `lto=thin`），否则 APK 体积不可接受
- ⚠️ 编译成功 ≠ 运行可用：Android 上的真机运行（步骤 6）尚未开始

---

## 6. 未完成 / 已知缺口（**别当成已完成**）

| 项 | 状态 |
|---|---|
| 步骤 4 Android 构建 | ✅ **已完成**（run 37409425912，artifact 354MB）——剩余：产物瘦身（strip） |
| 步骤 4 衍生 | ⚠️ `libcodex.so` 354MB 未 strip，APK 化前要瘦身（strip + panic=abort + lto） |
| 步骤 5 APK 外壳（jniLibs + app-server stdio + 桥接 + UI） | ❌ 未开始 |
| 步骤 6 真机验证（8 项） | ❌ 未开始 |
| 任务书步骤 2 的「重试与切换」 | ❌ **未实现**：`ContextOverflow→压缩重发`、`QuotaExhausted→切备用 provider` 的外层循环还没写（session 层有基础 HTTP 重试，但切换逻辑没有） |
| 旧版 `stream_chat_completions` 的 **auth 恢复循环（401→恢复→重试）** | ❌ 未复刻（当前 chat 路径 401 直接上抛） |
| `effort` / `service_tier` 透传 | ❌ 简化为不传 |
| 沙箱级验证（真机 8 项中第 5 项：3 个不同中转站） | ❌ 需真机 + 真实中转站 |

---

## 7. 云环境冷启动步骤

```bash
git clone https://github.com/rdztfxygcysnb-max/codex.git && cd codex-rs

# 1. 工具链（repo 的 rust-toolchain.toml 钉 1.95.0，editions=2024）
#    注意：系统自带 cargo（如 1.83）会直接报
#    "failed to load manifest for workspace member ... aws-auth" —— 这是本地工具链太旧，不是代码坏
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.95.0 --profile minimal
export PATH="$HOME/.cargo/bin:$PATH"

cargo metadata --format-version 1 --no-deps >/dev/null && echo "workspace OK"   # 快速自检

# 2. 跑测试（本机验证，约 15-40 分钟）
cargo check -p codex-api
cargo test  -p codex-api --lib chat
cargo test  -p codex-model-provider-info

# 3. 端到端隔离自测（无需 CI）
python3 scripts/mock_chat_relay.py &        # 127.0.0.1:8899
python3 scripts/deny_proxy.py > /tmp/proxy.log 2>&1 &   # 127.0.0.1:8890
export CODEX_HOME=$(mktemp -d)
printf 'model = "m"\nmodel_provider = "local"\n\n[model_providers.local]\nname="L"\nbase_url = "http://127.0.0.1:8899/v1"\nwire_api = "chat"\nenv_key = "K"\n' > $CODEX_HOME/config.toml
K=1 HTTPS_PROXY=http://127.0.0.1:8890 HTTP_PROXY=http://127.0.0.1:8890 NO_PROXY=127.0.0.1 \
  ./target/debug/codex exec --skip-git-repo-check --ephemeral "say hi"

# 4. Android 交叉编译
rustup target add aarch64-linux-android
export NDK_HOME=<ndk-r27c> TC="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CC_aarch64_linux_android="$TC/aarch64-linux-android26-clang" \
       CXX_aarch64_linux_android="$TC/aarch64-linux-android26-clang++" \
       AR_aarch64_linux_android="$TC/llvm-ar" \
       CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TC/aarch64-linux-android26-clang"
cargo build --release --target aarch64-linux-android -p codex-cli --bin codex
```

**config.toml 最小可用形态**（chat 中转站）：
```toml
model = "你的模型名"
model_provider = "relay"
[model_providers.relay]
name = "Relay"
base_url = "https://中转站域名/v1"
wire_api = "chat"
env_key = "RELAY_KEY"
[model_providers.relay.quirks]     # 可选，8 个旋钮
repair_tool_json = true
max_tokens_field = "max_tokens"
```

---

## 8. 诊断手册（本工程踩过的坑，按命中率排序）

1. **`cargo: failed to load manifest for workspace member ... aws-auth`**
   → 不是 aws-auth 坏。两种真因：本地 cargo < 1.95（edition 2024 不认）；或**某个 member 的 manifest 有 cargo 语义错**（python tomllib 能过、cargo 过不了，典型：`workspace=true` + `default-features=false`）。
   定位法：`cd <member> && cargo metadata --format-version 1 --no-deps` —— 报谁就是谁。
2. **CI 日志下载/解析陷阱**：步骤文件名含关键字会误命中（如 `10_Build ... isolation test.txt` 会被 `solation` 匹配到）。**用 `startswith` 精确匹配 `11_` 前缀**。
3. **跑几轮 CI 后长输出可能被本地摘要层压缩**——重要信息改成：写文件 → `grep` 关键词 → 短输出；或 base64 通道。
4. **GitHub fork 并发 = 1**：新 run 排队在旧 run 后面，排查时**主动 cancel 旧 run**（`POST .../runs/{id}/cancel`）。
5. **网络抖动**：`git push` 偶发 `OpenSSL SSL_read: unexpected eof`，**重试循环 5 次**即可；GitHub API 偶发 handshake timeout，重试。
6. **沙箱 `git` 曾全坏**：`/etc/gitconfig` 里 `createObject = copy` 是非法值（只认 link/rename），git 读到当场 fatal → 已改 `rename`（备份 `/etc/gitconfig.bak-20261005`）。
7. **PRoot 删不掉特殊文件**（`Operation not permitted`）——属正常，不影响。

---

## 9. 验证规则（不可违反，来自任务书）

1. `RepairOutcome::Unrecoverable` 的 tool call **绝不执行**（回工具错误让模型重发）；流未 `ended_cleanly()` **不调 finish()**。
2. 每条验证必须标 **「真机实测」/「单元测试」/「未验证」**；**禁止用 mock 结果冒充真机通过**。
3. API key **不进**仓库/日志/截图/报告，只走环境变量。
4. 上游友好：**少改现有文件**，新逻辑进新文件，便于 rebase；每步单独 commit。
5. 只有三种情况停：要移除/绕过安全机制、涉及凭据存储、需要破坏性操作。
