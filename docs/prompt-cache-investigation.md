# Prompt 前缀缓存调查：opencode zen 网关（GLM-5.3-Flash）

记录 2026-09-10 对提示词前缀缓存失效问题的排查过程与受控实验结论，
供日后回归对照。

## 背景与现象

- 配置：`[[providers]] name = "opencode"`，`base_url = https://opencode.ai/zen/go/v1/chat/completions`，
  `session_header = "x-opencode-session"`；主用模型 `glm-5.3-flash`（Anthropic 兼容上游，
  tool-call id 形如 `toolu_*`）与 `deepseek-v4-flash`（OpenAI 风格上游，id 形如 `call_*`）。
- 现象：WebUI 状态栏缓存命中（`cached_tokens`）长期为 0；供应商面板显示前缀缓存持续失效。
- 日志中 memory agent（recall / fork write pass）流量与缓存失效在时间上高度相关。

## catus 侧排查结论：请求路径无前缀改写

逐点核实过（对应回归测试见下）：

- 系统提示词字节稳定（`App::build_system_prompt`，技能目录刻意不变，禁用技能也保持目录内容）。
- 回合循环内消息 append-only：`start_llm_stream` 先快照再挂 assistant 占位符；
  所有运行期写入（工具结果、skill 注入、memory 注入、子代理结果）都只在尾部追加。
- 请求结构稳定：tools 顺序固定、`session_header` 值每进程一次（`new_session_id`）。
- 回归契约测试：
  - `app::tests::request_prefix_is_byte_stable_across_tool_rounds`
  - `app::tests::request_prefix_is_byte_stable_across_interaction_pause`
  - 助手 `llm::api_request_messages`（`#[cfg(test)]`）序列化 API 可见消息用于逐字节前缀断言。

日志侧旁证：同一会话内 prompt token 逐轮增量与新增消息完全吻合（append-only），
miss 却在单一流内部、间隔数秒、无插队流量时出现。

## 受控实验（直连网关，脚本在 `/tmp/opencode/cache_probe{1..7}.py`）

固定载荷：约 2.6k token 系统填充 + 单词回复；流式 + `stream_options.include_usage`；
`x-opencode-session` 可控。

| 实验 | 形态 | 结果 | 结论 |
|---|---|---|---|
| probe1 | 相同前缀、不同 session | 跨 session 命中 2624/2624 | 缓存按前缀内容键控，session id 不参与 |
| probe2 | 单 session、无 tools、9 轮增长 | 9/9 命中 | 基线正常 |
| probe3 | 单 session + tools | 7 命中 1 miss | 出现偶发 miss |
| probe4 | main/fork 交错 + reasoning + tools（全生产形态） | **0/9 全 miss**（含字节级完全相同的重复请求） | 时间窗口性失效 |
| probe5 | probe4 去掉 reasoning_content | fork 3/3 命中、main 0/6 miss | 混杂，排除单一变量 |
| probe6 | 同一请求 × 4 个 session × 重复 | 跨 session 命中、同 session 偶发 miss | 再证与 session id 无关 |
| probe7 | 单 session + tools + **reasoning_content** | **9/9 命中** | catus 请求形态（含 reasoning、tools、fork 结构）完全可缓存 |

### 综合判定

1. **缓存按前缀内容（账号级）键控，与 session id 无关**——fork / memory 子代理使用
   独立 session id（`subagent.rs` 中 `{id}-{millis}`）不会导致全价重付，无需让 fork
   复用 parent session id。
2. **catus 侧请求形态本身完全可缓存**（含 reasoning_content 与 tools）；生产中
   assistant 历史携带 `reasoning_content` 不是失效原因。
3. **间歇 miss 是网关侧多实例路由 / 缓存不稳定**，miss 率随时间在数分钟内从 0% 波动
   到接近 100%（probe4 全 miss 窗口 vs probe6/7 全命中窗口）。同一字节请求命中率
   如此波动，只能由"按请求随机路由到缓存隔离的后端实例"解释。
4. memory agent 与缓存的"相关性"是**流量放大**造成的观感：`auto_write` 每回合发起
   8~10 个 8.5k→13.4k 全前缀请求，是最大的缓存读写流量源，最先暴露网关波动；
   无因果改写路径。

## 已落地的配套改进

- **usage 去重**（`llm.rs::stream_chat`）：部分网关重复上报两条相同 usage 块，
  客户端只透传第一次，修复状态栏/`/status` 双倍计数。
- **观测增强**（`llm.rs`）：
  - `sse usage [session=…]` 日志行携带请求归属（main=`ses_*`、fork/recall=`subagent-*`）；
  - `LlmClient::log_request_fingerprint`（debug 级）：每请求输出 session、字节数与
    序列化 hash，便于与供应商面板逐请求比对。需 `agent.log_level = "debug"` 才可见。
- **session id 对齐平台格式**（`app.rs::new_session_id`）：`ses_<16hex><8hex>`，
  时间戳纳秒 + pid ⊕ ASLR 栈地址熵；进程内一次生成，`/resume` 沿用保存值，
  `/new`（或切换工作区）重新生成。

## 修复回归契约相关的其它改动（同批）

- `tool/edit.rs::diff_rows` 兜底分支越界 panic（`new_lines[173]`）：文件末行无换行
  且编辑在末尾追加/裁剪时触发；新增单侧耗尽分支与两个回归测试。
- `App::record_usage`/`stream_chat` 的 usage 去重回归测试：
  `llm::tests::stream_chat_dedups_duplicate_usage_chunks`（本地 mock SSE 服务器）。

## 后续建议

- 向 opencode zen 反馈：GLM-5.3-Flash 后端多实例路由无会话/前缀亲和，命中率随时间
  波动；可附本文件实验数据。缓存敏感场景可优先使用 `deepseek-v4-flash`
  （日志显示其前缀缓存命中稳定）。
- 复跑探针时注意：日志时间戳为 UTC（+8 = 本地），`catus.log` 中 main/fork/recall
  请求现可用 `session=` 字段区分。
