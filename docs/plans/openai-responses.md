# OpenAI Responses 协议

设置中可选择 OpenAI Chat Completions、OpenAI Responses 或 Anthropic Messages。沿用 rig-core 0.46.0 的 Responses HTTP/SSE wire，不引入另一套代理执行器。

## 配置与兼容

- `ApiProtocol` 替代设置中的 OpenAI/Anthropic 布尔值；现有 `openai` 标识仍指 Chat Completions，新增 `openai_responses`。未知保存值不会被 store 静默当成 OpenAI。
- 三种协议分别保存 Base URL、模型、思考等级和 Key 账户；旧版 `ai_model` 等键仍可读取，旧 `ai:openai:api_key` 不迁移。Responses 默认地址为 `https://api.openai.com/v1`，不会自动复制其他协议的 Key。
- 模型目录查询独立于协议标识：Responses 仍查 OpenAI 模型信息；模型输出和上下文设置按协议与模型分开保存。
- Responses 接受根地址、带代理前缀的 Base URL 或完整 `/responses` 地址，去掉多余尾斜杠，根地址补 `/v1`。代理前缀保留；误填 `/chat/completions` 或 `/messages` 会在保存/请求前拒绝。
- 连接测试、主代理、子代理、侧问及压缩都使用选定协议。不自动切换为 Chat Completions。端点 404/405 与明确的 `model_not_found` 使用不同错误提示。

## 请求与执行

- 使用 `input`、`instructions`、扁平函数定义、`max_output_tokens` 和 `reasoning.effort`；由 Rig 统一为现有流式事件。
- 函数工具明确发送 `strict: false`，保持现有可选参数语义。工具权限、主机资源、执行通道、持续服务和统一工具卡片沿用现有行为。
- 显式发送 `store: false`，无 `previous_response_id` 或 Conversations 托管会话。Rig 为无状态请求添加 `reasoning.encrypted_content`。第三方服务需支持这些字段；未返回加密推理条目时仍可使用语义历史继续。
- Responses 的服务端 `background` 与 Shellmate 的命令/子代理后台执行没有绑定。本版使用 HTTP/SSE，不接入 WebSocket、OpenAI 托管工具或服务端后台生成。
- 只有完整响应中的完整工具调用才能执行；参数截断、流中断和 failed 响应不会执行残缺调用。连接测试收到 incomplete 不报告成功。

## 历史与回放

`Entry::Assistant` 增加向后兼容的可选 `replay` 字段。原有文字、工具结果和 UI 投影不变；所有协议仍可读取旧记录。

Responses 的回放元数据包含同源模型标识、工具调用身份、助手消息 phase 和加密推理条目，存入 SQLite/子代理检查点；不保存明文推理、推理摘要或任意未知输出字段。调试格式隐藏整个 replay 字段内容。单条回放记录上限 2 MiB，超过时保留语义历史并重建上下文。

回放限定于相同协议端点、模型、系统说明、工具定义和未修改的历史前缀。输出压缩、结果更新、模型或端点变化使不兼容的加密状态失效，后续请求用可见文字与成对工具结果重建。工具 `call_id` 与结果保持对应；已执行步骤不会因为恢复上下文再次执行。旧 Chat/Anthropic 的原生推理仍仅存内存。

本地“保存对话”控制语义记录与这些回放数据；关闭保存不再写入新的历史，删除对话和子代理输出清理继续沿用原机制。`store: false` 控制远端 Response 对象存储，不改变请求需发送到所配置服务这一事实。

## 验证

脚本化 Responses SSE 覆盖正文/工具/用量、非严格 schema、路径规范化、端点错误、侧问、首事件/空闲超时、未启动流取消、截断与失败不执行命令、暂停恢复、立即处理追加消息、子代理继承、上下文变化后失效，以及 SQLite 关闭重开后的 phase/密文回传。原有 Chat Completions、Anthropic 和应用/存储回归继续执行。

真实 API 测试保留为显式 e2e：`SHELLMATE_AI_PROVIDER=openai_responses`，同时设置测试地址、模型和 `SHELLMATE_AI_KEY_FILE`，运行原有被忽略的测试。脚本化测试不代表任意第三方网关或真实模型一定兼容。

## 依据

- https://developers.openai.com/api/docs/guides/migrate-to-responses
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/docs/guides/conversation-state
- https://developers.openai.com/api/docs/guides/reasoning
- 本地锁定依赖 `rig-core = 0.46.0` 的 `providers/openai/responses_api` 请求、流式解码与回放实现。

## 实施验证（2026-11-27）

应用 831 项、AI 单元 37 项/agent 91 项/provider 28 项、store 单元 80 项/集成 28 项通过；未启用真实服务、钥匙串等依赖外部环境的忽略项。先前推送的计时测试断言遗漏结束时间与“运行”字样，已仅修正预期。新增协议原生样本补齐了场景计数。完整回归后补加的手动压缩与分段参数用例通过定向回归，SQLite 集成单独复验通过。workspace/all-target Clippy `-D warnings`、构建与 `git diff --check` 通过。

隔离原生样本 `set-ai-responses@980x620` 已检查亮暗主题、三个协议切换、Responses 模型/思考等级恢复及 Esc 关闭；没有向真实服务发送请求。运行日志位于 `/private/tmp/shellmate-responses-implementation/`。构建复用仓库 target，禁用增量编译；没有新建额外工作树或清理用户正在使用的构建目录。
