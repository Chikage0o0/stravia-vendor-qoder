# Stravia Vendor Qoder

将 Qoder 账号接入 [Stravia](https://github.com/Stravia-AI/StraviaPlatform) 的 Rust/Wasm 供应商插件。Stravia 负责客户端 API、路由和 canonical 协议转换，本插件通过原生 HTTP 调用 Qoder 上游服务，不启动 CLI 子进程，也不要求本机安装 Qoder CLI。

本插件不是 Qoder 或阿里巴巴官方发布的集成。上游属于非公开协议，没有稳定性承诺；请只使用自己的合法账号，遵守服务条款，不用于共享账号、转售额度或规避官方限制。推理可能消耗订阅额度或积分，实际计费以上游为准。

## 功能与边界

- 浏览器设备授权：使用 PKCE、独立 nonce 和稳定的机器标识，授权窗口为 5 分钟。支持令牌刷新；刷新时保留未轮换的有效字段并核对账号身份。
- CN / Global 两个 channel：按所选区域固定推理、OpenAPI 和数据政策中心地址；保存的凭据绑定区域，跨区域使用会在出站前被拒绝。
- 远端模型发现：读取账号的 `assistant` 场景目录，排除禁用条目和需要独立 BYOK 凭据的模型，映射名称、输入/输出限制、图片及思考能力。没有内置的假模型列表。
- 原生推理：保留 canonical 消息、工具调用、工具结果和思考参数；将 Qoder 外层 SSE 信封转换为 canonical 文本、推理、工具参数及 usage 增量。工具执行由客户端或宿主负责，不在插件内执行命令。
- 严格流终止：内部 `[DONE]` 或 `finish_reason` 不单独构成成功，必须收到上游 `event:finish`；终止事件到达后不再等待 HTTP EOF。断流和业务错误不能变成空成功。
- 额度查询：读取个人、附加、组织共享及专属资源包，分别显示上游提供的用量。借鉴 WorkBuddy，将总量放进额度项标题（如 `个人额度 · 总计 100`），剩余列保留实际积分值；CN 使用中文标题，Global 使用英文标题，专属包保留上游名称。总量缺失时标题显示 `—`，不把未知字段当作零余额，也不将不同资源包合成一个余额。额度 key、已用量、总量、百分比、重置时间及可用状态保持原有契约。
- 账号与套餐展示：额度详情通过 SDK `account_label` 显示上游可读身份，按邮箱、用户名、昵称、姓名依次选择；不使用账号 ID、令牌或组织名。标签取自登录和刷新时已有的 userinfo 查询，不增加请求。旧凭据缺少标签时仍可正常查询额度，后续正常刷新即可补齐，无需为此重新授权；当前资料不再提供身份时清除旧标签。套餐仅取额度响应的 `user_type`，复用官方展示规则将 `team_plus` 等机器标识整理为 `Team Plus`，不从余额猜测订阅。新版 SDK 的 `billing` 保持缺省：现有响应不能确认计费周期与固定价格，不根据套餐名或区域推测成本建议。
- 非生成式连接检查：读取所选区域固定的 `/api/v1/userinfo`，核对返回账号与保存的凭据一致，仅报告 `credentials` 已验证；不声称推理端点可用，不同步模型、不刷新令牌、不写入状态，也不发起计费推理。
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

1. 安装支持插件声明并编译专属出站形状的配套 Stravia 宿主；旧宿主不认识 `plugin:qoder-cli`，不会回退至内建形状。升级宿主后必须重建并重新导入组件。
2. 本地运行 `task dist`，在 Stravia 的供应商插件页面导入 `dist/stravia-vendor-qoder-v0.3.0.wasm`。本轮版本尚未发布；[历史 Releases](https://github.com/Chikage0o0/stravia-vendor-qoder/releases) 的组件使用其各自的宿主契约，不能替代当前本地构建。
3. 新建 **Qoder** 供应商连接，选择 `cn` 或 `global`。
4. 发起浏览器授权，打开返回的网址完成账户选择；超过 5 分钟需重新发起。
5. 授权完成后同步模型，再按 Stravia 的客户端 API 配置接入。

例如，使用 Stravia 默认统一入口：

```text
Base URL: http://127.0.0.1:23471/v1
API Key:  由 Stravia 访问控制配置决定
```

上面的 API Key 是 Stravia 客户端访问密钥，不是 Qoder 令牌。Qoder 凭据由 Stravia 宿主持久化；本插件不读取本机 CLI 认证文件，也不把上游令牌交给客户端。

## 版本对齐与真实验证

两条路径互相独立：本插件通过宿主 Core 网络栈调用上游，不启动 CLI；官方 CLI 捕获仅用于观察 CLI 本身，不能代替插件真实请求。以下均不要求重新登录 CLI，也不把已有 Stravia 凭据写入脚本或命令行。

### 插件路径

在已构建当前 Wasm 后，从本仓库运行：

```sh
python3 scripts/vendor_alignment.py \
  --data-root /path/to/stravia-data --scenario discover --output /private/qoder-report.json
# 明确确认产生上游请求及可能消耗额度后，才添加 --allow-live；推理可另加 --scenario infer --model MODEL_ID。
```

脚本从 `Cargo.toml` 读取 Stravia 依赖固定的 git tag（当前 v0.7.0），首次运行时稀疏浅克隆该 tag 的共享 harness 到 `target/host-<tag>/`，之后复用；不需要本地 `StraviaPlatform/` checkout。`--allow-live` 还须用 `--runner` 或 `STRAVIA_ALIGNMENT_RUNNER` 指定由同一 tag 宿主源码预先构建的 Core 单元测试可执行文件。组件默认 `target/wasm32-wasip2/release/stravia_vendor_qoder.wasm`，可用 `--component` 指定其他构建产物。场景可重复指定 `infer`、`media-image`、`discover`、`allowance`；是否支持以插件及上游实际结果为准。不带 `--allow-live` 只读清单、不联网；真实执行使用数据目录已有连接，不通过 CLI 登录、刷新或撤销凭据。报告中的请求一致性与真实上游业务成功应分别判断，认证通过、合成应答和 401 都不能证明真实推理成功。应将报告写入私有目录，并核对共享 harness 的实际输出边界。

### 官方 CLI 隔离捕获

使用 Linux 交互终端，需 `unshare`、`mount`、`ip`、`openssl` 和可用的 user/mount/network namespace。官方默认安装路径提供原生 standalone，和 npm 包在 Node 中运行是不同入口，不应混为同一指纹。下面使用官方 1.1.75 Linux amd64 发行物；脚本对提取后的可执行文件校验版本及固定 SHA-256，并要求 `--site` 与发行物一致：

- CN：官方[安装脚本](https://qoder.com.cn/install)指向 `https://static.qoder.com.cn/qoder-cli-cn`；[1.1.75 manifest](https://static.qoder.com.cn/qoder-cli-cn/channels/1.1.75/manifest.json) 中 `qoderclicn-linux-x64.tar.gz` 的 SHA-256 为 `55d3d947a9e67240677c0847d02252de5210abe2f0bf2debb9bcf58e2b0738cf`；提取出的 `qoderclicn` 为 `49d6e014df9fb636bf50d2c2ff68d4a42f6952c8de763e3b065ac2312948a17c`。
- Global：官方[安装脚本](https://qoder.com/install)指向 `https://qoder-ide.oss-accelerate.aliyuncs.com/qodercli`；[1.1.75 manifest](https://qoder-ide.oss-accelerate.aliyuncs.com/qodercli/channels/1.1.75/manifest.json) 中 `qodercli-linux-x64.tar.gz` 的 SHA-256 为 `6207478eda10efca0e2bea20c57c3c890c0cc82bd79c149be8f8c95931d72637`；提取出的 `qodercli` 为 `f64099d3d9516c4e324df775545c035877551bfd5b4d69650f90422acbcc9289`。

```sh
# 从相应官方 manifest 下载并校验压缩包、提取后，二选一：
python3 scripts/capture_official_cli.py --executable /path/to/qoderclicn \
  --site cn --version 1.1.75 --no-refresh-fixture --temp-parent /dev/shm \
  --output /private/qoder-cn-summary.json
python3 scripts/capture_official_cli.py --executable /path/to/qodercli \
  --site global --version 1.1.75 --no-refresh-fixture --temp-parent /dev/shm \
  --output /private/qoder-global-summary.json
# npm 包的不同 Node 入口仍可单独观察：
python3 scripts/capture_official_cli.py --package /path/to/extracted/package \
  --version 1.1.75 --node /path/to/node --output /private/qoder-npm-summary.json
```

脚本创建自动清理的私有临时 HOME（`--temp-parent` 可指定其父目录；`/dev/shm` 在可用时为 tmpfs），在独立网络命名空间只启用 loopback，并将官方域名指向本地合成 TLS 服务。终端内走默认交互 PTY：仅输入合成内容，选择登录及信任临时目录，完成后用 `/exit` 退出；不要粘贴密码或真实令牌。`--no-refresh-fixture` 不在合成设备授权中提供 refresh token，并对主请求及辅助请求提供直接合成应答；它**不限制请求次数**。默认不加该选项时的 Read 工具续接也是合成数据。

`--restore-readonly` 在第一轮合成登录退出后，对合成 auth 目录作只读 bind mount 并重启默认交互 CLI；第二轮也须退出。可同时加 `--sdk-auth-interactive`，仅在第二轮设置内容为 `{"type":"qodercli"}` 的无令牌 SDK payload，用于观察是否实际初始化该上下文；该选项要求 `--restore-readonly`。CN native 1.1.75 合成实测中默认交互没有消费该 payload，也没有建立 SDK 的 read-only/`refreshStrategy: none` 上下文，而是正常恢复本地会话。SDK 的 print/stream-json 路径与默认交互不同，不能据此声称等价；payload 读取器会记录前 32 字节，绝不能把凭据写进去。

报告按 connection ID 关联请求和 ClientHello，保留有序 TLS 套件、扩展编号及长度、支持版本和已知 ALPN；记录完整有序 HTTP 头名称，仅保留安全静态值和受限格式的公开 User-Agent，其余值脱敏。正文仅保留长度、SHA-256、字段 schema 和固定合成提示标记是否命中，不保存原文、查询串、令牌、终端记录或响应。提示可使用 `QODER_SYNTHETIC_CAPTURE_MARKER_8274`；`operation_context=main_agent_loop` 来源于合成日志与 trace 的关联，不能把长度启发式当作主请求证明。原生模式通过只记录版本的 preload 取得实际 Bun 及 Node 兼容版本；npm 模式记录实际 Node 版本与入口哈希。捕获错误或没有观察到 infer 返回非零；临时 CA 仅供隔离进程信任，不关闭 TLS 校验。

CN 和 global native 的一次合成交互各观察到主请求、标题和建议共三次推理请求；这是本地 fixture 的观察，**不证明线上请求次数、官方服务接受请求或插件真实推理成功**。脚本不提供可安全使用生产凭据的 live 入口：只读存储不能阻止刷新，现有全传输栈请求路径/次数 guard 尚未证明不可绕过，禁止将真实凭据交给此脚本。

可选 `--credentials-fd FD` 从继承的文件描述符读取仅含 `token`、`user_id` 的 JSON；两个值必须以 `synthetic-` 开头，只用于本地合成设备授权，不可传真实凭据。不要在参数、stdout、报告或仓库中放置密钥。正常退出或异常时临时目录会清理；被强制终止时请人工删除指定临时父目录内残留的 `qoder-capture-*`。

1.1.75 的合成登录会话已用官方 bundle 内嵌 WASM 离线解密并验证加密/解密往返：`QoderCredentialStorage` 以 `.qoder/.auth/machine_id` 的前 16 字符作为 `credential_storage_encrypt` / `credential_storage_decrypt` 的第二参数，明文是 JSON；此路径不是操作系统 keyring。合成会话观察到 `uid`、`name`、`security_oauth_token`、`access_token`、`refresh_token`、`expire_time`、`refresh_token_expire_time`、`login_method`、`login_timestamp`、`encrypt_user_info`、`key`、`startup_validation`。源码设备登录把 token 同时写入两个 token 字段，并使用秒级有效期；guest 的有效期为毫秒，转换必须考虑单位。CLI 从 `uid`、`security_oauth_token`、`organization_id`、`organization_tags`、`data_policy_agreed` 重生成运行时加密字段；guest 中相应值及 `identity` 不能简单当作整份 CLI 会话。

这仍不能证明 guest 到 CLI 会话的**无损导入**：合成会话不含 region，CLI 的 `Ik()` 从环境选择区域，登录后 `syncEndpointAfterLogin()` 还会调度端点选举；guest 不保存原 CLI 登录时间、名称等所有会话字段。正常加载本地 CLI 会话会启动刷新计时器，源码在到期前一小时即可触发刷新。不能猜区域或客户端身份，也不能为补足字段而刷新或撤销生产凭据。此脚本因此只允许合成 FD；插件真实验证应使用上面的共享 harness 和原有 Stravia 凭据。guest 身份涉及 `region`、`uid`、`subject_id`、`access_token`、`machine_id`、`organization_id`、`organization_tags`、`data_policy_agreed`、`identity`；有效期及刷新字段可能存在。上述离线往返只证明合成向量，不证明官方上游接受导入后的会话。

如需额外的单连接 TLS/HTTP 观察，可另行使用宿主的 `stravia-tools capture-client`（`--ca`、`--output`、`--client`、`--client-version`），它只接受 loopback 连接并输出脱敏观察。该工具为 localhost 签发证书、只处理一条连接；上述 CLI 合成服务和该工具不是同一个服务，不能以其输出声称捕获了 CLI 的完整多请求流程，亦不要绕过证书校验来连接它。

## 配置

| 字段 | 说明 |
|---|---|
| `machine_os` | 配置的客户端平台枚举：`x86_64_win32`、`aarch64_win32`、`x86_64_linux`、`aarch64_linux`、`x86_64_darwin`、`aarch64_darwin`。默认 `x86_64_win32`，不是自动探测结果。 |
| `machine_hostname` | 可选客户端名称。留空时使用 `DESKTOP-` 加持久化机器 UUID 的 SHA-256 前 7 位大写十六进制字符。自定义值按 CLI 的 ASCII / IDNA / 哈希规则规范化，出站最长 96 个 ASCII 字符；拒绝非字符串、CR/LF 和超过 4096 UTF-8 字节的输入。 |

这些字段是可覆盖的合成客户端身份，不是实际硬件信息。签名的模型目录、数据政策和推理请求带 `Cosy-MachineOS`，仅推理请求带 `Cosy-MachineHostname`。插件不读取真实 hostname、MAC、机器 ID 或架构，也不生成真实 UMID。无 UMID 的推理请求省略 `Cosy-MachineToken` / `Cosy-MachineType`；模型目录和政策签名 GET 则保留官方回退语义：机器 UUID 用作 `Cosy-MachineToken` / `Cosy-ClientIp`，`Cosy-MachineType` 为 `5`。这不是采集到的硬件令牌或 IP。

模型同步返回账号的可用 `assistant` 场景目录，不再提供连接级模型 ID 筛选；需要补充模型时，可在供应商的“可用模型”页面手动添加。模型发现不发起推理，也不根据余额猜测模型是否免费。宿主提供的管理员静态模型 ID 列表可覆盖远端发现；该列表只接受字符串 ID，相关可用性仍由上游决定。

### 连接检查

新版宿主的连接检查使用当前连接草稿中的配置与凭据，不要求先保存或同步模型。插件先校验区域、固定 Base URL、客户端身份配置及凭据内部一致性，再向区域 OpenAPI 发起一次受保护的 userinfo GET。成功仅表示保存的账号凭据通过验证，不表示推理服务、某个模型或额度可用；没有有效凭据时需先完成浏览器授权。

连接检查通过 SDK 必需的 `VendorGuest::check_connection` 接口实现，不再在供应商或渠道的可选 capability 中声明 `connection_check`。更新宿主后必须重新构建、导入插件，不能继续使用携带旧 capability 声明的组件。

账号 ID 按 `id`、`user_id`、`uid` 的首个非空字符串选择，与登录和刷新保持一致。非法 JSON、缺失或非法账号 ID、2xx 错误信封归为 `InvalidResponse`；返回其他账号归为认证错误。HTTP 401、403、429、503 等继续保留上游认证、权限、限流或服务不可用分类，不通过自动刷新或重试掩盖失败。检查不更新账号标签、套餐或任何凭据，也不把上游响应体回显到错误信息。

### 会话与请求 ID 契约

核对对象为官方 CLI `1.1.75`（插件固定版本）与 `1.1.76`；两者 ID 构造点（请求构造器、重试重生成、业务上下文）逐字相同，仅混淆名不同。npm 官方注册表当前 `latest` 为 [`1.1.85`](https://registry.npmjs.org/@qoder-ai/qodercli/latest)，本插件未宣称与该版本协议完全一致。官方所核对版本的 ID 使用 `node:crypto.randomUUID()`，即小写、带横线、36 字符的 UUIDv4；CLI 对会话 ID 的校验（`--session-id`、分叉目标）只要求 `8-4-4-4-12` 十六进制，不限定版本，也不区分大小写。

| 字段 | 官方生命周期 | 插件来源与行为 |
|---|---|---|
| `session_id` | 会话启动时新建，恢复会话沿用 | 确定性映射宿主 `operation_metadata.client_conversation_id`；缺失、空串或非字符串时新建随机 v4 |
| `request_id` = `chat_record_id` | 每个模型请求新建；CLI 内部连接重试重新生成 | 每次 Infer 新建一枚 v4，两字段同值 |
| `request_set_id` | 每次用户提示预分配，工具轮次、压缩、重试共用；子代理另建 | 确定性映射宿主 `operation_metadata.user_prompt_id`；缺失、空串或非字符串时新建随机 v4 |
| `business.id` | 主路径取同一提示的 `request_set_id`；特定回放路径独立新建 | 与本次 `request_set_id` 同值 |
| 签名 payload `requestId`、数据政策 `requestId` | 各自独立的 `randomUUID()` | 保持各自既有 profile，不参与会话转换 |

宿主在可信提示准入过程中生成会话和提示 ID，分别在同一会话及同一提示的多次模型调用间复用。插件分别计算：

```text
SHA-256(b"qoder-session-v1\0" || client_conversation_id 的完整 UTF-8 字节)
SHA-256(b"qoder-prompt-v1\0" || user_prompt_id 的完整 UTF-8 字节)
→ 各取前 16 字节，设置 UUID v4 版本位与 RFC variant 位
→ 小写、带横线的 36 字符 UUID
```

命名域末尾是真实 NUL 字节。不 trim、不改大小写、不解码十六进制；完整字节参与，两个域彼此独立。此映射是插件的确定性转换，不是官方算法；从旧版 `session_affinity` 切换来源会改变既有上游会话身份。缺少可信宿主字段时分别使用随机值，不从客户端自报的 canonical `parameters.session` 或用于传输复用的 `transport_affinity` 猜造身份。

每次推理还创建独立的 `business` 上下文，包含 CLI 产品/协议版本、`agent` 类型、`request_set_id` 同值的业务 ID、所选模型的 `model_key`、开始时间和 `start` 阶段。`model_key` 来自 1.1.75 默认交互 PTY 的合成主请求观察；较早的孤立构造器夹具未建立该生命周期。标题取当前提示的有效 Unicode 前缀，最长 10 个 UTF-16 单元，不生成半个代理对。该对象是上游业务路由所需的请求组成部分，不是可随意省略的日志字段；插件不额外调用业务上报服务。此对齐不证明任何真实上游 403 的原因或业务成功。

推理直接投影 canonical IR，不再借用会拆分和重排并行历史的 OpenAI 编码器。工具结果错误标记和显式缓存提示仅在输入 IR 中存在时保留；若入口 codec 已丢弃标记或将带提示的文本块折叠为字符串，插件无法恢复原始客户端 wire 信息。白名单外的请求选项静默忽略，不发送、不告警，也不因为这些选项拒绝推理；非法凭据、区域、配置和无法表达的实际消息内容仍明确报错。

宿主保存的响应分组标记仅用于恢复消息边界，不进入 Qoder 正文。同一响应内的 reasoning、文本和并行工具调用继续合并；不同响应，以及已标记响应与未标记客户端消息，不再因角色相同而合并。未标记的连续内容项保留既有 CLI 投影规则。

### 请求白名单

依据官方 CLI `1.1.75` 的完整正常链 `bg → u3e/BKl → XQA → Sft → HYi → hVl/IVl → E6n → sendRemoteChatAsk` 逐字段生成请求，不把入口请求或模型目录对象整体复制到上游：

- `parameters` 仅允许 `temperature`、`top_p`、`top_k`、`max_tokens`、`reasoning_effort`、`enable_thinking`、`reasoning_budget_tokens`、`preserve_thinking`、`context_length`、`tool_choice`。许可不代表当前 canonical IR 能表达每一项；现有入口映射 generation、reasoning、Anthropic `top_k` 和工具选择，保留既有输出上限及思考开关规则。
- `seed`、`stop`、`presence_penalty`、`frequency_penalty`、`response_format`、并行工具控制、其他协议扩展及工具声明的 `strict` / `cache_control` / `meta` 静默忽略。工具参数 JSON Schema、调用参数和 JSON 工具结果属于用户负载，不按字段名递归过滤；例如 schema 中名为 `seed` 的属性仍完整保留。
- 文本缓存断点映射为 CLI 的 `cache_control`；非自动尾断点的显式一小时提示保留 `ttl: "1h"`，自动尾断点覆盖为无 TTL 的 `{"type":"ephemeral"}`。不发送 canonical 的 `breakpoint_priority`。工具结果文本仅投影 `type` / `text`，图片仅投影 `type` / `image_url.url`，不复制额外字段。
- 思考激活遵循 SDK 的 `mode → level → budget_tokens` 优先级；显式禁用不会被预算重新启用，全部未指定时保留目录默认。档位先按模型支持列表筛选，不为被丢弃的档位额外发送激活参数。`temperature` / `top_p` 的零值保留，`top_k=0` 省略。
- 所有客户端请求头静默忽略，包括与原生签名、身份、模型头同名的头。出站头只由协议构造器、已验证凭据及配置的客户端身份生成；不会让客户端覆盖 `Authorization` 或 `Cosy-*`。

签名推理请求的显式请求头集合为：

| 类别 | 请求头 |
|---|---|
| 传输 | `Accept`、`Content-Type`、`Cache-Control`、`Connection`、`User-Agent: Bun/1.4.2`；不附加 Node/Undici 的 `Accept-Language` 或 `Sec-Fetch-Mode` |
| 签名 | `Authorization`、`Cosy-Date`、`Cosy-Key`、`Login-Version` |
| CLI 身份与业务 | `Cosy-Business-Product`、`Cosy-Business-Type`、`Cosy-ClientType`、`Cosy-Data-Policy`、`Cosy-MachineId`、`Cosy-MachineOS`、`Cosy-MachineHostname`、`Cosy-Scene`、`Cosy-User`、`Cosy-Version` |
| 条件字段 | 非空组织身份的 `Cosy-Organization-Id` / `Cosy-Organization-Tags`；非空模型键的 `X-Model-Key`，来源已提供时的 `X-Model-Source`（显式空字符串保留） |

模型目录、数据政策、OAuth 和额度请求继续使用各自逐字段构造的请求头与请求体，不接入客户端头透传。Fetch 默认 `User-Agent` 为 `Bun/1.4.2`；显式 JSON OpenAPI 请求保留 `qoder/1.1.75`，授权轮询使用 Fetch 默认值。业务字段按原生 Bun 顺序排序，随后追加缺省运行时字段；推理的 `Host` 与 `Content-Length` 使用空 slot，由宿主原位填写，不能由客户端参数透传。

官方 CN / Global standalone CLI `1.1.75` 在隔离 user/mount/network namespace 中分别通过默认交互入口、合成登录和本地 TLS 服务采集；运行时现场读取均为 Bun `1.4.2`、Node 兼容版本 `26.3.0`。主请求通过提示标记与日志操作关联识别，不按首个请求或正文长度猜测。两区主请求的 TLS 向量及字段顺序一致，使用 HTTP/1.1；插件内嵌的 `assets/qoder-cli.json` revision 2 保存该基线，选择 `plugin:qoder-cli`，而不是 npm 包在 Node.js `24.21.0` 上的不同形状。脱敏夹具分别保存两区来源摘要和连接关联；合成应答不代表生产成功。此前 npm 路径的工具续接、session / request set 生命周期观察仍只证明其原场景。官方可选 OpenTelemetry 活跃上下文会注入 `traceparent`；宿主没有对应上下文时插件不伪造该字段。

全部 HTTP 请求使用 SDK Net 接口：插件显式设置 `Accept-Encoding`，普通 HTTPS / 推理请求使用 `gzip, deflate, br, zstd`，认证的模型目录和政策签名 GET 使用 `identity`；`Options.response_decoding` 分别声明相应解码集合或空集合。响应编码校验和流式解压由宿主负责；插件不手写 `Content-Encoding` 或长度值，也不包含 HTTP 压缩或解压实现。Qoder 的私有 `encode_body` / base64 封装及其签名仍由插件处理，它们不是 HTTP 内容压缩。

重建的实际 Wasm 组件契约覆盖公共 HTTP 导入、设备授权、模型同步、Responses 文本响应、工具调用及工具续接，并独立校验请求签名和 ID 生命周期。宿主 TLS 检查直接捕获其生成的 ClientHello，与两区原生采样比较；这与完整的官方客户端／插件同 socket 业务对照不是一回事。可选 tracing、组织信息、数据政策及配置的平台身份仍存在差异。2026-11-09 使用授权来源的已有凭据和保存模型 metadata，修正 native 形状后的 CN／Global 真实推理仍返回上游 403，不能标为成功对齐；没有重置身份、刷新凭据或以合成应答替代。

### 缓存键与会话键对照

以下对照以官方 CLI `1.1.75` 的完整正常构造链为基线，不以孤立缓存函数或通用 `chat.proto` 的可选字段代替最终发送行为：

| 字段 / 机制 | 官方 CLI | 当前插件 |
|---|---|---|
| `session_id` | 正常构造链接收会话值 | 宿主 `client_conversation_id` 的确定性转换；缺失时随机新建 |
| `request_id` / `chat_record_id` | 当前请求共用同一 ID | 已发送同一本轮随机 UUID |
| `request_set_id` | 请求组独立于请求；正常 Agent 生命周期在工具轮次间复用 | 宿主 `user_prompt_id` 的确定性转换；缺失时随机新建 |
| `business.id` | 主路径取该提示的请求组 ID，特定调用可使用独立预分配值 | 与本轮 `request_set_id` 同值 |
| `agent_id` / `task_id` | 业务路由字符串，不是会话 UUID | 保留正常路由 `agent_common` / `common`，不拿宿主链路 ID 替代 |
| 签名 payload `requestId` | 认证签名协议的请求标识 | 保留独立的本次签名 UUID，不将其误认为会话键 |
| 内容 `cache_control` | 字符串预处理后选择自动断点，再投影消息和图片 | 已实现普通文本自动断点及显式一小时文本提示；图片最终不带标记 |
| `source_session_id` | 按实际分支或恢复状态条件传递来源会话 | 没有对应 CLI 来源会话状态时不发送；不拿父节点或客户端自报值冒充 |
| `custom_context` / `patches` | 正常构造器支持可选上下文及补丁 | 未建立相应 CLI 上下文/补丁状态，不透传任意对象 |

宿主提示准入字段提供会话与用户提示的可信生命周期；插件分别映射它们，不从 Observation、宿主父节点或客户端自报值拼造身份。宿主未提供对应字段时仅在本次 Infer 内生成随机 ID；插件不声称复刻 CLI 完整 Agent 生命周期。

自动缓存断点按官方正常预处理及 `HYi` 的 `skipCacheWrite=false` 路径处理：

1. 字符串用户内容先归一为文本块，空 user 省略，连续 user 按正常链合并。再选择最后一条非 system/developer 历史，在拆分工具结果前处理。
2. 从该消息末尾向前选择第一个非 thinking、redacted thinking、tool use、tool result 的块。canonical reasoning 块作为 thinking 处理。只在这条消息内查找，不回退到更早消息。
3. 选中文本时发送 `{"type":"ephemeral"}` 并覆盖该断点原有 TTL；更早的显式一小时标记保留。选中普通图片时，后续 `hVl` / `wco` 重建 `image_url` 会丢弃缓存字段，不能改为标记更早文本。选中空文本时，后续转换丢弃空文本，同样不能挪动断点。

CLI 的 `skipCacheWrite=true` 侧路依赖其工作流状态；当前 canonical Infer 不具备该状态，插件不新增客户端自报控制键或假造侧路请求。不支持的图像格式按官方前置降级规则变为占位文本，非视觉模型的图像降级则保留其后置时序；两者不能混用缓存规则。

正常原生链没有独立的 OpenAI/Responses 风格缓存键；这些入口字段不出站。非原生的 `max_completion_tokens` 别名处理已删除，入口 codec 转为 canonical `max_tokens` 后才参与原生投影。发布包 `chat.proto` 虽定义了 `cache_id`，核对的正常构造链未观察到赋值，不额外发送。CLI tracing 头依赖其遥测上下文；插件不伪造这些值。

是否命中上游缓存、缓存 TTL 或计费收益不能由静态字段判断；本次离线验证不调用真实账号，也未修改宿主会话接口。

## 构建与验证

五个 Stravia 依赖（`stravia-vendor-sdk`、`stravia-vendor-common`、`stravia-runtime-contract`、`stravia-protocol-codec` 及 dev 依赖 `stravia-vendor-runtime`）通过 git tag 固定到 [`StraviaPlatform` v0.7.0](https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v0.7.0)，由 Cargo 直接拉取，无需本地 `StraviaPlatform/` checkout，`Cargo.lock` 锁定到该 tag 的具体提交。升级基线时同步修改 `Cargo.toml` 中五处 `tag`，核对 SDK、protocol-codec 与 vendor-runtime 契约，并通过构建及真实组件验证。

品牌图标由 `assets/qoder.svg` 编译进 Wasm，无需运行时下载。它使用 [Qoder 官方 SVG](https://qoder.com/favIcon.svg) 的原始路径，去掉底色并转为透明单色，以适配 Stravia 的浅色和深色主题。显示图标需要同时使用更新后的宿主并重新导入插件；归属及授权边界见 [NOTICE](NOTICE)。

依赖 Rust `1.99.1`、`wasm32-wasip2` target，以及可选的 [Task](https://taskfile.dev)。工具链由 `rust-toolchain.toml` 固定。

当前源码面向宿主 Vendor `1.0.0`、Net `1.1.0` 和 State `1.1.0` 接口，并依赖支持插件专属出站形状声明的 SDK 和宿主，配套基线为 [`StraviaPlatform` v0.7.0](https://github.com/Stravia-AI/StraviaPlatform/releases/tag/v0.7.0)；此前源码基线 `037b07ad1d80fa35c55bd68a6f1cfa3e664b55a2` 本身不足以满足此要求。须使用匹配宿主重新构建并导入组件；历史标签仍使用各自固定的依赖。本次不改变插件原有精确域名声明。

HTTP 响应编码声明保序及首次去重修复已进入上游 [`ff6db7f4`](https://github.com/Stravia-AI/StraviaPlatform/commit/ff6db7f4)，包含在 v0.7.0 中，不再依赖未发布的工作区补丁。实际传输仍须使用包含该修复的宿主；重建插件不能替换旧宿主的 HTTP 实现。

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

离线形状验证须先构建上述 release Wasm 和由 `StraviaPlatform` v0.7.0 源码构建的 `stravia-tools`（`stravia-devtools` 包），显式指定工具可执行文件：

```sh
STRAVIA_TOOLS=/absolute/path/to/stravia-tools task verify-egress-shape
```

该任务读取本仓库 `tests/fixtures/qoder-cli.json` 和 `tests/fixtures/qoder-cli-global.json`，分别以 `gateway.qoder.com.cn` 和 `api2.qoder.sh` 合成本地 ClientHello 并对照捕获；不连接上游、不读取凭据。更新形状时维护 `assets/qoder-cli.json` 并用两份有来源的原始捕获重新验证；采集脚本必须显式指定 `--output`，不得覆盖捕获来源或将真实凭据交给脚本。

当前 Stravia runtime 接受受限接口集合中的稳定 WASI `0.2.x` 版本，包括 SDK 与 Rust 标准库的混合 patch 版本；接口函数和资源类型仍须匹配。组件契约测试加载实际产物验证这些导入，不修改宿主白名单或二进制导入名称绕过检查。

以下为旧 dev 基线上的历史离线验证，不代表当前接口迁移后的结果；当前迁移尚须重新构建及验证：

- 当时 dev 适配通过 34 项单元测试；保留官方 `1.1.65` 的全合成密码学向量，并覆盖思考激活优先级、目录档位筛选、缓存及非空思考签名边界、会话 ID 固定向量、请求组 ID 生命周期和可读账号选择。
- 当时通过 10 项真实 Wasm 组件契约，覆盖两区域登录、刷新、模型、额度、原生签名、反序并行工具结果及任意字节切块 SSE；正文夹具保持官方 `1.1.75` 正常链输出。账号与套餐展示、旧凭据无标签、刷新不沿用过期标签、明确授权拒绝与暂时错误分类、空白 refresh token 回退和过期时间保留均继续通过；身份边界另覆盖凭据轮换后会话不变。两项连接检查契约通过新版必需接口验证两区域账号 ID 优先级与回退、只读副作用边界、畸形响应、账号变化、HTTP 错误分类、错误脱敏，以及凭据、区域、地址和配置错误的出站前拒绝。消息边界契约新增不同响应及已标记/未标记历史场景：旧投影先复现错误合并，修复后通过，并保留同响应内容合并。
- 当时使用固定 Rust `1.99.1` 构建并运行独立 `protocol_smoke`，通过 stdin 提供全合成凭据和 HTTP/SSE 响应，完成 Responses 字符串入口推理，返回 `DEV_SYNC_83613F47_OK` 和 14 token 合成 usage；出站验证稳定会话、请求组 ID、`br, gzip, deflate` 压缩声明，以及对象工具 Schema 内的 `true` / `false` 和 `additionalProperties: false` 不被改写。连接检查由上述真实组件契约验证，本次独立入口未执行登录或连接检查。Responses 入口不接受根级布尔 `parameters`，该限制不等于 Qoder 上游拒绝布尔 Schema；本插件不套用 Antigravity 的专有转换。示例构建因直接包含契约测试文件产生未使用导入和测试辅助项警告，不影响运行。
- 协议对齐阶段曾通过 20 组 SDK 等价输入与官方黄金向量的 JSON 语义差分；随机 ID、时间和传入会话归一化，业务标题由官方 `E_a` 独立计算。实际出站同时检查 Fetch 默认头、三种响应编码策略及稳定合成身份。该历史差分本次未重新执行。
- 配套宿主在此前协议对齐阶段通过 6 项 HTTP 编码/解码测试及 2 项真实 TCP 编码协商测试，覆盖声明保序、首次去重、空列表 `identity`、响应白名单、截断及流式边界；本次未重跑这些宿主测试。Wasm mock 回复不冒充宿主 HTTP 解压验证。

官方原始场景保存在 `tests/fixtures/qoder_cli_1_1_75_body_cases.json`。差分中的显式缓存使用直接 SDK `CacheControl`，因为本轮 Anthropic 入口转换已将该单文本块折为字符串；非标准 `JPG` MIME 也使用直接 SDK 媒体输入，不声称 Anthropic wire 接受它。SDK 没有对应的历史清除控制，等价 CLI 输入不提供 `generation.reasoning.preserve=false`。完整合成输入、正文、差分范围及组件 SHA-256 保存在本地生成的 `target/qoder-cli-1.1.75-parity-report.json`，其中不保存认证请求头。该报告对应协议对齐时的发布前构建，下载附件的校验和以对应 Release 的 `SHA256SUMS` 为准。这些验证没有访问生产账号。

独立组件 smoke 已运行，包含固定版本宿主的原始 `credential_bundle_from_response` 函数和 `CredentialBundle` 类型：旧 `0.1.5` 产物因缺少标准访问令牌被拒收，修复版通过凭据接收及元数据 JSON 往返，并继续完成模型、额度、中文文本、推理、分片工具参数和 usage。组件回归同时确认刷新时不同的 canonical 账号仍被拒绝；缺少 `event:finish` 和跨区域凭据也被拒绝。这部分验证不调用真实账号。

`0.1.8` 已通过真实 Stravia 桌面客户端加载并测试 CN `qfmodel`（Qwen3.8-Flash）：同账号、同消息、同参数下，旧 `0.1.7` 返回 HTTP 200 内的业务 400 `oa_qwen-plus-main / Execution failed: null`；只补充 `business` 后返回 `QODER_OK` 和真实 usage。临时恢复旧产物仍失败，再恢复修复版再次成功，排除了期间上游自行恢复的解释。真实 SSE 工具调用也完成参数分片及正常终止；241 条合成消息、108 次工具调用/结果、54 组并行调用和反序结果，在 `reasoning_effort=medium` / `max_tokens=16384` 下正确返回 `alpha/beta`。该历史使用 3088 输入 token，不是原始约 107394 token 的私有内容重放；Global 和其他模型没有真实账号验证，非公开协议未来仍可能变化。

## REA 逆向分析工具（开发用途）

项目根目录的 `.mcp.json` 为 Windows 下的 OMP 注册了 [REA](https://github.com/morluto/rea) MCP 服务，通过 `npx` 运行固定版本，不修改用户全局代理配置。需要符合 [REA 运行要求](https://github.com/morluto/rea/blob/main/docs/installation.md) 的 Node.js 与 npm；Rust 构建和 Wasm 插件不依赖此工具。

`.agents/skills/reverse-engineer-anything/` 包含该 npm 发行包配套的技能与参考资料。安装后重启当前项目的 OMP 会话，使其发现 MCP 服务和技能。CLI 也可直接使用：

```powershell
npx --yes --ignore-scripts rea-agents@6.7.0 --help
```

启动配置使用 `--ignore-scripts`，避免可选 Frida 依赖的原生安装脚本阻塞下载。Frida 原生绑定不在本次安装范围内；需要动态插桩时须单独完成安装。原生反编译还需自行配置 Ghidra 或 IDA 等分析引擎；注册 MCP 不代表所有分析后端均已就绪。

不带参数的 `rea doctor` 执行全局审计，检查用户级客户端注册、技能和分析引擎；本项目仅使用项目级配置，因此该命令仍可能报告用户级注册缺失和 `healthy: false`。项目接入应通过 `.mcp.json` 中的启动命令完成 MCP `initialize` 与 `tools/list` 验证，不应为消除全局审计提示而改写用户配置。

## 协议来源与维护

当前客户端身份固定为已核对的官方 CLI [`1.1.75`](https://registry.npmjs.org/@qoder-ai/qodercli/-/qodercli-1.1.75.tgz)。显式 OpenAPI User-Agent、`Cosy-Version`、签名 payload 和推理业务版本同步使用该值；普通 Fetch 请求使用官方传输链的 `undici` 默认 User-Agent。

npm `1.1.85` 包的离线静态检查显示其 `$br` 请求构造器仍将 `request_id` 与 `chat_record_id` 设为同值，并读取 identity 中的 `requestSetId` 和 `sessionId`；这只验证该构造点，不证明新版完整生命周期或其他协议字段一致。REA JavaScript 包分析 Evidence：`ev_1e34751ee52e7bb400adc0b1821ad9a7d3800e4bc62804e9bbd7c3f2a8ba11c6`；未运行 CLI，也未访问生产服务。

本实现以 `1.1.75` 发布包的定向源码、隔离官方函数执行和实际 Wasm 差分为依据。`bundle/qodercli.js` 文本 SHA-256 为 `1b5b17bb25bcd2e059a97e8bc1d4280e4c16fb4194b004463cd677c434536948`；历史认证向量保留其原始 `1.1.65` 版本，不冒充新版向量。生产组件不分发、嵌入或执行官方认证 WASM，也不运行 CLI bootstrap。

最终传输核对覆盖正常 `sendRemoteChatAsk → X8i → KYe.send → za → HFi → H_A → FT → YSn → h9i → FD.fetch`，不是仅凭 bundle 中存在某个 HTTP 库推断默认值。官方 FT 用合成 HTTPS dispatcher 执行，物理网络和文件系统访问被拒绝；新版图片缓存的早期孤立函数结论已撤回，以前述完整链规则为准。可选 `custom_context` / `source_session_id` / `patches` 仍须有真实 Agent 状态才发送，不构造替代值。

这些核对证明已覆盖输入下的 HTTP 应用层字段与 JSON 语义，不是整包或全 Agent 等价证明。JSON 成员序列、HTTP 头序列化格式、TLS / ALPN、HTTP 版本和连接管理不承诺与 CLI 相同；不能保证服务端无法区分，也不能由静态字段推断真实缓存命中、TTL 或计费收益。

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

### 0.3.0

- 五个 Stravia 依赖由 `../StraviaPlatform/backend/crates/...` 本地路径改为 git tag 依赖，固定到 `StraviaPlatform` v0.7.0（`Cargo.lock` 锁定 `7b9274f6`）；发布工作流不再检出主仓库，`STRAVIA_PLATFORM_REF` 变量随之移除。`scripts/vendor_alignment.py` 不再使用 `--host-repo` / `STRAVIA_PLATFORM_DIR`，改为按 `Cargo.toml` 的同一 tag 稀疏浅克隆共享 harness。已在该基线上通过 34 项单元测试和 10 项真实 Wasm 契约；未访问真实账号。

### 0.2.0

- 同步 Stravia dev `83613f4752178e952226fc21a386c3c79a4e3131`：五个路径依赖升级为 `0.6.2`，锁文件对齐新版宿主映射镜像缓存依赖，发布工作流固定相同基线；WIT 保持 `stravia:vendor@0.6.0`，Rust 仍为 `1.99.1`。旧 WIT `0.5.1` 宿主不能加载新版组件；新版宿主也不接受旧 `connection_check` capability，须重新构建并导入插件。不新增网络权限。
- 将非生成式连接检查迁至 SDK 必需的 `VendorGuest::check_connection`，移除描述符 capability 和旧 `execute` 分支：仍仅通过区域固定 userinfo 核对保存的账号，成功报告 `credentials`，不冒称推理端点已验证；不刷新、不模型同步、不写状态、不发起推理。协议错误、账号变化、HTTP 分类与错误脱敏由真实组件边界验证；非法配置及不一致凭据在出站前拒绝。
- 原生推理按宿主响应分组标记保留 assistant 轮次，避免不同响应或已标记/未标记历史被拼接；同响应的思考、文本和并行工具调用继续合并，内部标记不进入上游正文。真实 Wasm 边界用例先复现错误合并，再通过修复。
- 适配新增可选额度 `billing` 字段，保持未知，不根据 `user_type` 或区域猜测计费方式、周期和固定价格；账号标签、套餐展示和额度数值保持不变。
- 同步官方插件的账号与套餐展示：仅保存 userinfo 的首个可读身份为可选 `account_label`，优先邮箱，不增加网络请求或保存完整资料；旧凭据等待正常刷新即可补齐。套餐复用官方 `plan::display_name`，保留上游人类可读名称与全部额度数值语义。
- 对照 Antigravity 的 [OAuth 修复 `d379a106`](https://github.com/Stravia-AI/StraviaPlatform/commit/d379a106)，设备令牌端点复用官方 `oauth_token_error`，显式 OAuth 拒绝与 HTTP 401 归认证错误，403 权限错误、429、503 及缺少必需令牌的成功响应不冒充撤销。刷新返回空白、空串或 null 的 refresh token 时保留原令牌，不再误报 `vendor authentication failed`。
- 核对 Antigravity 的套餐、身份与布尔 Schema 修复：Qoder 套餐继续来自自身 `user_type`，不引入 Google 专有端点；保留已发布的 Qoder UUID 映射与本轮请求组契约，不改成 FNV 或 `agent/UUID`。组件回归验证凭据轮换、插件重载和客户端伪造字段不改变可信会话身份。
- 本次通过 34 项单元测试、10 项真实 Wasm 契约和独立 `protocol_smoke` Responses 协议冒烟，返回 `DEV_SYNC_83613F47_OK` 与 14 token 合成 usage；此前两项刷新边界的修复继续通过。本次未访问真实账号、未验证桌面页面；使用时须升级配套宿主并重新导入组件。

### 0.1.13

- 对齐官方 CLI 的会话与请求 ID：`business.id` 改为与同次 `request_set_id` 同值（官方 TUI 与 SDK 主路径的 `Sin(... id: r||djA())`），此前是独立随机 UUID。核对 `1.1.75` 与 `1.1.76` 的 ID 构造点一致；`session_id` 的稳定映射、`request_id` / `chat_record_id` 同值及每次推理重新生成的行为不变，宿主没有请求级可信源时不伪造稳定值。
- `session_id` 映射 `SHA-256("qoder-session-v1\0" || session_affinity)` 现作为稳定身份契约写入文档，并以固定向量、Unicode/CRLF/NUL、重载一致性和客户端伪造字段隔离的测试固定。修改该算法会使所有已有上游会话身份发生变化。
- 删除添加供应商时的“自定义模型 ID”配置及其校验、目录筛选和中英文文案。模型同步保留全部可用目录结果，补充模型使用供应商“可用模型”页面的手动添加功能。
- 更新插件后，旧连接若保存过 `vendor_options.model_ids`，需要通过管理 API 更新连接，显式提交已移除该键的 `vendor_options`，并保留仍需使用的 `machine_os`、`machine_hostname`；没有其他选项时提交空对象。宿主不会自动清理未知字段，普通连接编辑也不能移除它；省略 `vendor_options` 会继续保留旧值，旧键残留时配置校验会失败。

### 0.1.12

- 配套 Stravia dev 基线更新为 `a97469a5a577f61eaa0b5f262005c9ee3603b8c6`，五个 Stravia crate 统一升级为 `0.5.2`，Vendor WIT world 为 `stravia:vendor@0.5.1`；Rust 工具链和最低版本同步为 `1.99.1`，发布工作流使用相同 SDK 基线。宿主保序补丁仍是本轮工作区改动，尚未随该上游提交发布。
- 客户端身份更新为官方 CLI `1.1.75`；普通 Fetch 补齐 `undici`、`Accept-Language` 和 `Sec-Fetch-Mode`，显式 OpenAPI UA 保持优先。按请求类别补齐版本、客户端类型和配置平台字段，修正模型头的缺失/空值条件。
- 默认 hostname 改为机器 UUID 派生的稳定 `DESKTOP-` 合成名称，不读取真实硬件；保留官方非推理签名 UUID 回退，推理不伪造 UMID。PKCE verifier 使用官方 43–128 长度与字符集，刷新体保持 `refresh_token`、`machine_id` 顺序且不补造机器令牌。
- 全部 HTTP 调用使用 SDK 显式压缩入口，普通请求声明 `br, gzip, deflate`，模型和政策签名 GET 声明空列表，由宿主发送 `identity`。配套宿主保留声明顺序、首次去重及严格编码校验；不增加 Wasm HTTP 压缩实现，保留 Qoder 私有封装与签名。
- 正文按官方完整正常链投影：普通字符串归一后参与自动缓存，保留非尾断点一小时 TTL，处理空/连续用户消息、图片规范化及降级、错误工具图片和空工具参数。撤回此前基于孤立缓存函数得出的图片标记结论；普通图片最终仍不带标记。空思考签名不出站。
- 思考激活遵循 SDK mode、level、budget 的优先级；显式禁用不被预算重新启用，未指定时保留目录默认，不支持的档位不新增激活字段。保留采样零值，省略 `top_k=0`。
- 通过 30 项单元测试、6 项真实 Wasm 契约、宿主 8 项 HTTP/协商测试及 20 组等价 SDK 输入差分；独立 `protocol_smoke` 返回 `SDK_DEV_OK` 和 14 token 合成 usage。未访问真实账号服务；使用新版须重建配套宿主并重新导入组件。

### 0.1.11

- 配套 Stravia dev 提交更新为 `7fac0bc5af644ae13c389b219700a9c4359181af`（SDK 仍为 `0.4.0`）；最低 Rust 版本及固定工具链同步升至 SDK 要求的 `1.99.0`，发布工作流使用相同提交。
- 推理投影适配统一 canonical 请求控制：从 `parameters.top_k` 读取采样值，使用 `reasoning.level` / `mode` / `budget_tokens`，不再依赖旧 `ProtocolExt`、`reasoning.effort` / `enabled` 或已移除的工具附加字段。Qoder 白名单边界不变，不能表达的原生工具与工具子集选择不出站。
- 保留 Responses 同轮 reasoning、文本和并行工具调用的原生消息合并及反序工具结果回归，不把 canonical 内容项误当作独立 assistant 轮。
- 依赖升级至 Stravia dev SDK `0.4.0`，同步本地与 CI checkout 布局；组件契约迁移至 owned canonical 文本、显式编码执行接口与 WebSocket reuse-age 参数。额外客户端请求头声明保持为空，不扩大权限。
- 客户端身份更新为 Qoder CLI `1.1.70`，同步所有原生身份与签名字段；保留官方 `1.1.65` 的独立密码学向量，组件契约检查签名、请求头与业务版本的一致性，不固定发行版本默认值。
- 发布前使用固定 Rust `1.99.0` 工具链和配套宿主提交完成 release Wasm 构建，通过 21 项单元测试与 6 项真实 Wasm 组件契约；独立运行 `protocol_smoke` 加载组件，以全合成 stdin HTTP/SSE 响应返回 `RELEASE_0_1_11_OK`，usage 总计 14 token。本轮未调用真实账号服务；使用新版须重新构建并导入插件。

### 0.1.10

- 在 Provider 描述符中内嵌官方来源的透明单色 SVG，不再依赖官网 favicon 获取品牌图标。
- Stravia 依赖切换为配套本地 checkout；迁移当前 SDK 的可选授权 state 和 channel 描述符字段，保留设备授权语义。
- 发布工作流默认固定配套宿主提交，保持本地构建与 CI 的 SDK 版本一致。
- 本地通过 21 项单元测试、5 项真实 Wasm 组件契约；独立加载打包产物确认版本为 `0.1.10`、CN / Global channel 及内嵌 SVG 均可读取。本轮验证未调用真实账号服务。

### 0.1.9

- 调整额度展示：CN 的个人、附加、组织共享及未提供名称的专属包使用中文标题；各项标题显示自身总量，未知总量显示 `—`。保留独立额度项和稳定 key，不改变剩余值、重置时间或耗尽保护语义。
- 白名单外的请求参数从拒绝请求改为静默忽略；请求头保持原生逐字段构造，所有客户端头均不透传。
- 将显式文本缓存标记映射为 CLI 的 `type: ephemeral`，移除 canonical 私有缓存字段和工具结果内容中的额外传输字段，保留工具 schema 与结果负载。
- 将当前宿主 GenerationChain 派生的 `session_affinity` 转换为稳定的原生会话 UUID；为每轮请求、请求组和业务分别生成官方格式 ID，不将会话或父节点冒充请求组/来源会话。
- 按官方 `DlA` / `Fgi` / `FWc` / `UWc` 实现自动缓存断点，覆盖工具结果拆分、助手工具调用、图片和空文本边界；删除非原生输出上限别名处理。
- 新增两区域真实 Wasm 组件回归，覆盖非 CLI 参数、客户端签名/身份覆盖尝试、稳定会话与独立请求组、自动缓存断点及合法请求语义；修正此前将旧固定宿主行为套到当前宿主的分析。
- 发布前通过 21 项单元测试、5 项真实 Wasm 组件契约，并独立加载组件确认描述符版本为 `0.1.9`；本轮验证未调用真实账号服务。

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
