# Stravia Vendor Qoder

将 Qoder 账号接入 [Stravia](https://github.com/Stravia-AI/StraviaPlatform) 的 Rust/Wasm 供应商插件。Stravia 负责客户端 API、路由和 canonical 协议转换，本插件通过原生 HTTP 调用 Qoder 上游服务，不启动 CLI 子进程，也不要求本机安装 Qoder CLI。

本插件不是 Qoder 或阿里巴巴官方发布的集成。上游属于非公开协议，没有稳定性承诺；请只使用自己的合法账号，遵守服务条款，不用于共享账号、转售额度或规避官方限制。推理可能消耗订阅额度或积分，实际计费以上游为准。

## 功能与边界

- 浏览器设备授权：使用 PKCE、独立 nonce 和稳定的机器标识，授权窗口为 5 分钟。支持令牌刷新；刷新时保留未轮换的有效字段并核对账号身份。
- CN / Global 两个 channel：按所选区域固定推理、OpenAPI 和数据政策中心地址；保存的凭据绑定区域，跨区域使用会在出站前被拒绝。
- 远端模型发现：读取账号的 `assistant` 场景目录，排除禁用条目和需要独立 BYOK 凭据的模型，映射名称、输入/输出限制、图片及思考能力。没有内置的假模型列表。
- 原生推理：保留 canonical 消息、工具调用、工具结果和思考参数；将 Qoder 外层 SSE 信封转换为 canonical 文本、推理、工具参数及 usage 增量。工具执行由客户端或宿主负责，不在插件内执行命令。
- 严格流终止：内部 `[DONE]` 或 `finish_reason` 不单独构成成功，必须收到上游 `event:finish`；终止事件到达后不再等待 HTTP EOF。断流和业务错误不能变成空成功。
- 额度查询：读取个人、附加、组织共享及专属资源包，分别显示上游提供的用量。未知字段保持未知，不把缺失数据当作零余额。
- 错误分类：将登录过期、模型排队和重复请求映射为规范的认证、限流和不可重试请求错误；错误不回显凭据。重试及刷新时机由宿主决定，插件不自动重试或切换模型。

撤销只清理本地待登录状态并返回 `Revoked`，由 Stravia 清理连接凭据；它不等于在 Qoder 服务端吊销已经签发的令牌。

## 区域与地址

| channel | 推理与模型目录 | OpenAPI（登录、刷新、额度） | 数据政策中心 | 浏览器授权 |
|---|---|---|---|---|
| `cn` | `https://gateway.qoder.com.cn` | `https://openapi.qoder.com.cn` | `https://gateway.qoder.com.cn` | `https://qoder.cn` |
| `global` | `https://api2.qoder.sh` | `https://openapi.qoder.sh` | `https://center.qoder.sh` | `https://qoder.com` |

连接的 Base URL 使用表中的推理 origin，不附加 `/algo` 或 API 路径，也不接受任意地址覆盖。插件声明的额外网络 origin 仅用于实际 OpenAPI 和数据政策 HTTP 请求；浏览器授权网址不是插件的 HTTP 出站请求。

插件查询账户数据政策，不写入上游政策设置。请求中的数据政策字段按官方 CLI 约定映射：`AGREE` / `NO_RECORD` 为 `agree`，`DISAGREE` 为 `disagree`。

## 安装与使用

1. 安装 Stravia。
2. 本地运行 `task dist`，在 Stravia 的供应商插件页面导入 `dist/stravia-vendor-qoder-v0.1.8.wasm`。维护者发布后，也可从本仓库 Releases 下载对应版本并用 `SHA256SUMS` 校验。
3. 新建 **Qoder** 供应商连接，选择 `cn` 或 `global`。
4. 发起浏览器授权，打开返回的网址完成账户选择；超过 5 分钟需重新发起。
5. 授权完成后同步模型，再按 Stravia 的客户端 API 配置接入。

例如，使用 Stravia 默认统一入口：

```text
Base URL: http://127.0.0.1:23471/v1
API Key:  由 Stravia 访问控制配置决定
```

上面的 API Key 是 Stravia 客户端访问密钥，不是 Qoder 令牌。Qoder 凭据由 Stravia 宿主持久化；本插件不读取本机 CLI 认证文件，也不把上游令牌交给客户端。

## 配置

| 字段 | 说明 |
|---|---|
| `model_ids` | 可选，每行一个远端模型 ID。填写后仅同步目录中匹配的模型，留空时同步账号的可用 `assistant` 场景目录。不会从输入配置凭空生成模型。 |
| `machine_os` | 配置的客户端平台枚举：`x86_64_win32`、`aarch64_win32`、`x86_64_linux`、`aarch64_linux`、`x86_64_darwin`、`aarch64_darwin`。默认 `x86_64_win32`，不是自动探测结果。 |
| `machine_hostname` | 可选客户端名称。留空时使用 `stravia-` 加持久化机器 UUID 的前 12 个十六进制字符。自定义值按 CLI 的 ASCII / IDNA / 哈希规则规范化，出站最长 96 个 ASCII 字符；拒绝非字符串、CR/LF 和超过 4096 UTF-8 字节的输入。 |

这些字段是可覆盖的客户端身份，不是实际硬件信息。签名的模型目录、数据政策和推理请求带 `Cosy-MachineOS`，仅推理请求带 `Cosy-MachineHostname`。插件不读取宿主 hostname、MAC 或架构，不把 UUID 伪装成真实 UMID，也不虚构 `Cosy-MachineToken` / `Cosy-MachineType`。

模型发现不发起推理，也不根据余额猜测模型是否免费。宿主提供的管理员静态模型 ID 列表可覆盖远端发现；该列表只接受字符串 ID，相关可用性仍由上游决定。

同一宿主会话通过 `session_affinity` 维持上游会话亲和；没有亲和元数据时使用本轮独立 ID。每轮生成独立请求 ID，不使用客户端自报的会话标识。

每次推理创建独立的 `business` 上下文，包含 CLI 产品/协议版本、`agent` 类型、独立 UUID、开始时间和 `start` 阶段。标题取当前提示的有效 Unicode 前缀，最长 10 个 UTF-16 单元，不生成半个代理对。该对象是上游业务路由所需的请求组成部分，不是可随意省略的日志字段；插件不额外调用业务上报服务。

推理直接投影 canonical IR，不再借用会拆分和重排并行历史的 OpenAI 编码器。工具结果错误标记仅在输入 IR 中存在时保留；若入口 codec 已丢弃标记，插件无法从原始客户端 wire 恢复。无法表达的内容或生成选项明确报错，不静默忽略。

## 构建与验证

依赖 Rust `1.98.1`、`wasm32-wasip2` target，以及可选的 [Task](https://taskfile.dev)。工具链由 `rust-toolchain.toml` 固定。

```bash
# 构建组件
cargo build --locked --release --lib --target wasm32-wasip2
# target/wasm32-wasip2/release/stravia_vendor_qoder.wasm

# 构建并打包组件、许可证和校验和
task dist

# 单元测试
cargo test --locked --lib

# 真实组件契约：须先构建 release Wasm
cargo test --locked --test component_contract -- --ignored --nocapture

# 或统一运行
task test
```

固定的 Stravia runtime 只接受 WASI `0.2.9` 接口。`Cargo.toml` 对 WASIp2 target 精确约束 `wasip2 = "=1.0.3"`，与 Rust 标准库及宿主 ABI 对齐；不要单独升级到 WASI `0.2.12` 绑定，也不要通过修改宿主白名单或二进制导入名称绕过检查。

离线验证分两层：

- 单元测试对照官方 CLI `1.1.65` 的全合成认证 WASM 向量，覆盖 AES/RSA、请求编码、签名、思考开关、错误分类和额度边界。
- 组件契约加载实际 Wasm，通过本地全合成响应覆盖两区域登录、刷新、模型、额度和任意字节切块的 SSE，并独立解码请求、验算签名。并行工具历史和业务上下文夹具来自官方构造器及正常 `AgentLifecycle` 的合成执行，覆盖反序结果、错误标记和客户端身份覆盖。

独立组件 smoke 已运行，包含固定版本宿主的原始 `credential_bundle_from_response` 函数和 `CredentialBundle` 类型：旧 `0.1.5` 产物因缺少标准访问令牌被拒收，修复版通过凭据接收及元数据 JSON 往返，并继续完成模型、额度、中文文本、推理、分片工具参数和 usage。组件回归同时确认刷新时不同的 canonical 账号仍被拒绝；缺少 `event:finish` 和跨区域凭据也被拒绝。这部分验证不调用真实账号。

`0.1.8` 已通过真实 Stravia 桌面客户端加载并测试 CN `qfmodel`（Qwen3.8-Flash）：同账号、同消息、同参数下，旧 `0.1.7` 返回 HTTP 200 内的业务 400 `oa_qwen-plus-main / Execution failed: null`；只补充 `business` 后返回 `QODER_OK` 和真实 usage。临时恢复旧产物仍失败，再恢复修复版再次成功，排除了期间上游自行恢复的解释。真实 SSE 工具调用也完成参数分片及正常终止；241 条合成消息、108 次工具调用/结果、54 组并行调用和反序结果，在 `reasoning_effort=medium` / `max_tokens=16384` 下正确返回 `alpha/beta`。该历史使用 3088 输入 token，不是原始约 107394 token 的私有内容重放；Global 和其他模型没有真实账号验证，非公开协议未来仍可能变化。

## 协议来源与维护

本实现以官方 [`@qoder-ai/qodercli@1.1.65` 发布包](https://registry.npmjs.org/@qoder-ai/qodercli/-/qodercli-1.1.65.tgz) 的静态源码和全合成离线互操作结果为依据。生产组件不分发、嵌入或执行其认证 WASM。

研究对照包括 [qodercli2api](https://github.com/Liki4/qodercli2api)、[qoder-proxy](https://github.com/avaritiachaos/qoder-proxy) 和 [qoder-cli-api](https://github.com/onehub-work/qoder-cli-api)。社区实现与官方当前版本的差异需分别核实，本仓库未直接复制 AGPL 实现源码。

官方入口：[CLI 脚本模式](https://docs.qoder.com/cli/run-in-scripts)、[Agent SDK](https://docs.qoder.com/cli/sdk/overview.md)、[认证说明](https://docs.qoder.com/cli/authentication)。SDK 包装 CLI 进程的方式与本插件的原生 HTTP 方式不同。

上游互操作协议固定使用 AES-CBC、RSA PKCS#1 v1.5 和 MD5 签名；这些选择不是新设计的通用安全协议，不应在其他场景复用。传输依赖 HTTPS，安全熵失败时明确报错，不使用伪随机降级。

Release workflow 由 `v*` tag 触发，也可在 GitHub Actions 的 Run workflow 中填写现有版本标签手动重跑。工作流始终检出该标签，版本必须与 `Cargo.toml` 一致；构建后先运行单元测试和实际 Wasm 组件契约，再发布附件。已有同名 Release 附件会保留，避免 CI 重建覆盖人工发布且已真实验证的产物。构建命令本身不发布、不推送。附件包含组件、`SHA256SUMS`、`LICENSE`、`NOTICE` 和第三方组件许可证。

## 项目结构

```text
src/lib.rs          VendorGuest 入口、区域与准入校验
src/profile.rs      身份、能力、配置及网络声明
src/auth.rs         PKCE 设备授权、刷新与凭据边界
src/protocol.rs     原生身份加密、请求编码与 COSY 签名
src/models.rs       远端模型目录、元数据与配置校验
src/inference.rs    RemoteChatAsk 与权威 SSE 终止
src/allowance.rs    账户及组织额度
src/state.rs        待登录私有状态
messages/           zh-CN / en-US 界面文案
tests/              真实 Wasm 组件契约
vendor/             消息编译器与第三方许可证
```

## 变更记录

### 0.1.8

- 修复推理业务 400：正常 CLI 调用会创建 `AgentLifecycle.businessInfo`，低层 builder 允许省略并不意味着模型节点允许省略。原生请求现包含真实本轮操作的 `business` 上下文，不增加遥测上报、自动重试或模型切换。
- 将官方合成夹具从孤立 builder 扩展到正常业务生命周期；旧组件在该契约上失败，修复组件通过。
- 经真实桌面客户端完成旧版失败 / 新版成功的正负对照，以及 SSE 工具调用、241 条合成消息和反序并行工具结果验证。

### 0.1.7

- 直接从 canonical IR 构造官方请求，保留并行调用分组、顺序、工具结果错误标记和思考参数，避免中间 OpenAI 编码器的语义损失。
- 按官方白名单投影模型和生成参数，添加用户 `contents`、工具调用 `index`，不发送 `stream_options` 或未定义的目录字段。
- 新增可覆盖的 `machine_os` / `machine_hostname`，提供明确的配置默认值，不声称自动取得真实硬件信息。
- 此版本的头和历史格式修正仍未解决真实业务 400；缺少 `business` 的问题由 `0.1.8` 修复。

### 0.1.6

- 修复浏览器授权后误判账号变化：按官方 CLI 的 `id`、`user_id`、`uid` 顺序选择 userinfo 中第一个非空字符串身份，保留登录和刷新时的账号一致性校验。
- 修复宿主无法接收授权凭据：统一返回和读取 Stravia 标准 `access_token`，不再使用插件私有令牌存储键；Qoder 加密负载中的协议字段不变。
- 将账户资料夹具改为同时包含不同的 `id` 和 `uid`，覆盖原先会触发 `vendor authentication failed` 的场景。
- 使用固定版本宿主的原始凭据转换函数进行独立离线接收验证；不读取真实令牌、不调用真实账号服务。

从 `0.1.5` 或更早版本升级到包含此修复的版本后，需要重新发起浏览器授权。旧授权链接绑定旧会话且有 5 分钟有效期，不能作为升级后的登录入口。网页显示“登录成功”只表示浏览器端完成；插件仍需完成用户资料、组织标签和数据政策查询，再将凭据交给宿主。已有 `0.1.6` 或更新版本的有效凭据可在升级到 `0.1.8` 时沿用，无需因为本次推理修复重新授权。

### 0.1.5

- 完整切换为 Qoder 原生 HTTP 供应商，提供 CN / Global 授权、模型发现、额度和推理。
- 按官方 CLI `1.1.65` 实现身份加密、COSY 签名、模型路由与思考控制。
- 统一构建、打包、界面文案和真实组件契约，并固定宿主兼容的 WASI 绑定。

## License

[MIT](LICENSE) · Copyright (c) 2026 Chikage0o0。第三方及上游边界见 [NOTICE](NOTICE)。
