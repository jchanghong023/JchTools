# CodeAgent 需求（CA）

**状态：CONFIRMED。** 本次用户通过一次封闭选择明确确认全部 CA-01～CA-42 并实施。本文唯一维护独立 CodeAgent 工具的行为、OMP provider 同步及验收条件。适用全局约束引用 [CONTRACT.md](CONTRACT.md)，协作、可信基及执行授权引用 [../../AGENTS.md](../../AGENTS.md)。本域不是 ACP HTTP 模型服务、截图后台或共享 Xberg 后台，不复制或改变其他域要求。

本文将用户提供的行为逐项编号为 CA-01～CA-42；确认不扩大 fulltest、slowtest、真实桌面、真实模型或发布权限。第 3 节的源码事实与第 4 节的验收计划不表示产品已经实现或验证通过。

## 1 独立工具、后台与设置

- **CA-01** 本文全部编号行为须经用户明确确认后才能实现；草案登记不授予产品修改或扩大验证范围的权限，确认结果另行记录，不伪造确认日期或已批准状态。
- **CA-02** JchTools 增加独立 CodeAgent 工具，在工具注册表注册并接入真实 GUI 页面；不把它作为现有模型服务、截图或 Xberg 页面的隐含功能，不改变其他工具的既有行为。
- **CA-03** 本工具新增业务、后台、凭据处理及配置同步使用 Rust；不引入 Python、Node.js、PowerShell 等凭据解密或业务代理层。既有开发/验证脚本可继续用于编排，不要求为本工具改写，也不额外限定验证夹具的语言。
- **CA-04** CodeAgent 由当前 Windows 用户拥有的独立、非提权、脱离 GUI 生命周期的后台进程执行；不需要管理员权限、不触发提权，也不作为系统服务运行。
- **CA-05** 主程序提供独立 `--codeagent-service` 入口，后台入口不启动主 GUI；生命周期实现参照已有 ACP runtime 模式，但不是复用 ACP 业务后台或 HTTP 服务。
- **CA-06** 后台具有实际状态查询、当前用户与登录会话范围内的控制命名管道和单实例约束；同一用户会话内重复启动或并发连接不能产生多个 CodeAgent 同步后台，不扩大到其他用户或登录会话。
- **CA-07** 打开 GUI 时异步确保 CodeAgent 后台已启动或连接现存实例；启动、连接、保存及同步不得阻塞 GUI，也不把“尝试启动”显示为实际运行成功。
- **CA-08** 关闭窗口或终止 GUI 后后台继续按配置运行与同步；重新打开 GUI 连接原实例，不重建已有后台。后台重新启动后恢复持久设置，不依赖上次 GUI 的内存。
- **CA-09** 不增加 Windows 登录自启，不提供 HTTP 监听、模型代理或推理服务；本工具只维护指定 OMP provider 配置，不将 ACP 模型服务的网络与 Agent 功能搬入本域。
- **CA-10** 同步间隔由 GUI 编辑，首次默认 60 秒；间隔与模型设置持久化到既有 `config.sqlite3` 的 `app_settings`，不是另建独立文本设置文件。
- **CA-11** GUI 允许维护一个或多个模型，每个模型可编辑 `id`、`name`、`reasoning`、`supportsTools`、`contextWindow`、`maxTokens`；后台使用已保存的完整模型列表，不固定只同步一个模型。
- **CA-12** 仅在本工具设置首次初始化时创建默认模型：`id` 与 `name` 均为 `DeepSeek-V4-Flash-0731-CodeAgent`，`reasoning: true`、`supportsTools: true`、`contextWindow: 200000`、`maxTokens: 32768`。用户保存后的模型、字段和间隔在重开 GUI 或后台后保留，不反复覆盖为默认值。
- **CA-13** GUI 保存异步立即持久化设置，并通知现有后台应用新设置及执行同步；不等下一次定时轮询才应用，不要求用户另按“重启”或“同步”才能使已保存设置生效。
- **CA-14** 持久化成功、通知或后台应用结果、OMP 文件同步结果分别真实报告；保存失败不能报已保存，保存成功不能冒称同步成功，后台未连接或同步失败不得被“保存成功”掩盖。
- **CA-15** 页面展示实际后台状态、最近同步时间以及同步“成功 / 未变化 / 失败”结果；状态来自真实后台与执行结果，不用已保存配置推算运行状态，不把未执行、失败或仅部分目标成功报为全部成功。

## 2 凭据、目标发现与无损同步

- **CA-16** 生产凭据只读取当前用户 `%USERPROFILE%/.cac/.credentials.json`；不读取其他用户凭据、不扫描或导出用户私有配置，不在文档、测试、日志中展示实际凭据。
- **CA-17** 只从 JSON 的 `idaasOAuth.xAuthToken` 取得加密凭据，对该值执行 base64 解码；缺失、错误格式或解码失败作为经过脱敏的同步失败，不伪造 token 或回退其他凭据字段。
- **CA-18** 在 Rust 中以 Windows `CryptUnprotectData` 的 CurrentUser 上下文、无可选 entropy 解密，并严格按 UTF-8 解码；解密失败或非法 UTF-8 失败，不使用有损替换文本，也不改用 LocalMachine 解密或外部命令。
- **CA-19** 每轮同步取得当前凭据，使凭据变化能更新目标 header；不能永远复用启动时 token。凭据读取或解密失败不以旧 token 冒报该轮同步成功。
- **CA-20** 每一轮重新发现 OMP 默认 `~/.omp/agent` 和每个当时存在的 `~/.omp/profiles/<name>/agent` 目标，覆盖运行期间新增的现存 profile；不凭空创建 profile，也不只在启动时缓存目录集合。
- **CA-21** 目标解析遵守实际 OMP 源码核实的 `PI_CONFIG_DIR`、`PI_CODING_AGENT_DIR`、`OMP_CONFIG_ROOT` 规则及 profile 关系；不能凭变量名猜测含义、优先级或支持范围。源码证据及具体路径算法在核实后留于实现说明，不另立未观察的产品路径规则；未核实的路径规则保持未核实，不冒称产品支持。
- **CA-22** 每个目标遵守源码核实的 `models.yml` / `models.yaml` loader 优先级，更新实际生效文件；不为规避现有文件另建遮蔽配置，不给不存在的 profile 生成配置，也不把文件存在等同于 loader 使用。
- **CA-23** 只同步 `providers.codeagent` 中本域明确负责的字段；其他 provider、顶层内容以及该 provider 内非目标字段保持不变。不删除、替换或重建整个 `providers` 映射以实现同步。
- **CA-24** `providers.codeagent.baseUrl` 固定为 `https://snapengine.cida.cce.prod-szv-y.dragon.tools.huawei.com/api/v2`，`api` 固定为 `openai-completions`，`auth` 固定为 `none`；不能改成 HTTP 代理地址、其他 API 或其他认证模式。
- **CA-25** `providers.codeagent.headers.x-auth-token` 为该轮解密的 token，`disableStrictTools` 为 `true`；只改目标 header，其他 header 属于非目标内容，必须保留。
- **CA-26** `providers.codeagent.models` 来自 GUI 持久化的完整模型列表，字段和值按 CA-11～CA-12，不把用户模型替换为测试模型，不暗中改变能力与容量。
- **CA-27** 对实际目标字段作语义比较；已等价时报告“未变化”，不得写文件、原子替换、触碰 mtime 或仅为了格式规范化重写。token、模型或其他目标语义变化时才产生相应更新。
- **CA-28** 保留所有非目标字节、注释、编码和换行，包括其他 provider、CodeAgent 的非目标字段、其他 header，以及目标 provider 内在修改 token / 模型 / 目标字段时仍须保留的全部既有注释。不能以“功能等价”代替字节守恒，不能把目标块内注释视为可丢弃内容。
- **CA-29** 不采用全文件 YAML 反序列化后重新序列化的同步方式；只对能安全定位的目标进行必要局部修改，未改区域逐字节保留，不全局排序、格式化或统一换行与编码。
- **CA-30** YAML 歧义或无法安全完成上述修改时保留原文件并报告脱敏失败；允许拒绝不支持的结构，但不得静默降级为破坏性重写，也不得用只支持简单夹具来宣称所有实际目标同步成功。
- **CA-31** 有变化时使用目标同目录临时替换文件，写入完成并 flush 后执行 Windows 原子替换；临时文件仅用于该目标的原子落位，失败时清理，不另建持久明文 token 副本、备份或导出。
- **CA-32** 读取、解密、解析、写入、flush 或原子替换等失败不得破坏目标原件；失败路径保留原始字节并清理本轮临时文件，不用截断原文件再写的方式实现更新。
- **CA-33** token 及其他秘密不得出现在日志、GUI、控制台、状态/管道响应、SQLite 或额外明文存储中；目标 OMP YAML 中用户明确要求的 header 与 CA-31 必需的短暂替换文件之外，不复制或持久化秘密。错误信息必须脱敏，不回显凭据、包含 token 的 YAML 内容或底层泄密诊断。
- **CA-34** 本工具不发起出站网络请求，不测试供应商连通性、不请求模型、不上传配置或日志；固定 URL 只是 OMP YAML 的配置值，写入它不授予调用它的权限。

## 3 验收、交付边界与源码事实

- **CA-35** 所有测试、隔离配置与临时输出由 `python scripts/make_tmp.py workspace` 在仓库 `.tmp/` 内创建，不能操作真实凭据、真实 OMP 配置、私人 profile 或资料来验证同步。
- **CA-36** 用当前用户 Windows DPAPI 生成的合成凭据验证 base64、CurrentUser / 无 entropy、严格 UTF-8 和凭据变化；覆盖缺字段、无效 base64、解密失败与非法 UTF-8，不能用固定明文桩代替真实 DPAPI 链路。
- **CA-37** 在隔离 OMP 根中按源码规则验证默认目标、多个现存 profile、运行期间新增 profile、相关环境变量及 `models.yml` / `models.yaml` 优先级；同时断言不创建虚构 profile、不产生遮蔽生效文件。
- **CA-38** 经真实后台公开入口验证单实例、当前用户会话管道、GUI 退出后的定时同步、重新连接、保存通知即时应用、多个模型与全部字段、间隔以及后台重启后的持久恢复；局部纯函数通过不代替进程生命周期验证。
- **CA-39** 隔离测试须验证语义未变化时字节与 mtime 均不变、token 和模型变化、其他 provider / 非目标字段 / 全部注释 / 编码 / 换行逐字节守恒，以及歧义、文件访问与原子替换失败时原件完整、临时文件清理和错误脱敏。
- **CA-40** 本任务只执行获授权的 Windows 本地定向测试与非桌面 smoke；不得隐含运行 fulltest、slowtest、真实桌面、发布或真实模型请求。未执行的验收项如实标为未验证，不削弱测试或扩权来制造通过；必要基线更新按可信基独立提交，不削弱既有断言。
- **CA-41** 产品实现与可信基（含本需求、AGENTS、静态检查及必要基线）变更按既有纪律分别提交，提交记录真实确认结果及变更理由；不得混为一次产品加合同提交。用户本次已授权 Git 提交与推送，但这一交付授权不代替 CA-01 要求的逐项行为确认。
- **CA-42** 新域同步登记需求索引、AGENTS 权威清单及 `scripts/static_check.py` 的 `_REQUIREMENT_DOCS`；产品阶段完成工具注册与页面接入，不将登记、哈希或静态检查冒充功能验收证据。

### 3.1 实现事实说明（非合同条目，不是产品验收）

本节只记录观察到的匹配版本源码事实，不新增、批准或改变 CA 行为。源码 checkout 是只读 oracle，不编译、不运行其配置加载器、不读取用户实际配置或秘密；具体实现仍须满足 CA-20～CA-33。源码研究不构成用户确认或 CodeAgent 实现证据。

| 事项 | 已观察事实 / 证据 |
|---|---|
| 匹配版本 | 主负责人报告本机 OMP 为 `18.9.2+fork.307`，经 release API 定位 `jchanghong023/oh-my-pi` 提交 `9f33db8955ce466da2bd3195c757bc4d719c7308`；专门源码研究已按该提交核实下列事实 |
| 配置根与环境变量 | 该版本确实支持 `OMP_CONFIG_ROOT`、`PI_CONFIG_DIR`、`PI_CODING_AGENT_DIR`；有效绝对 `OMP_CONFIG_ROOT`（先 trim 与 `~` 展开）覆盖 home / `PI_CONFIG_DIR` 根，空值与相对 root 回退；`PI_CONFIG_DIR` 默认 `.omp`。证据：[dirs.ts](https://github.com/jchanghong023/oh-my-pi/blob/9f33db8955ce466da2bd3195c757bc4d719c7308/packages/utils/src/dirs.ts) 118～129、314～323、718～727 |
| 默认目标与 profiles | `PI_CODING_AGENT_DIR` 只覆盖默认 agent 目录，按 `path.resolve` 解析；named profile 忽略该 override，使用解析后根的 `profiles/<name>/agent`。证据：同一 `dirs.ts` 355～370、475～498、629～632；不能把 default override 当 profile 根 |
| models YAML 优先级 | 默认 modelsPath 为 agent 目录的 `models.yml`；存在 `.yml` 时优先，仅其不存在时使用 `.yaml` sibling。证据：[model-registry.ts](https://github.com/jchanghong023/oh-my-pi/blob/9f33db8955ce466da2bd3195c757bc4d719c7308/packages/coding-agent/src/config/model-registry.ts) 579；[config-file.ts](https://github.com/jchanghong023/oh-my-pi/blob/9f33db8955ce466da2bd3195c757bc4d719c7308/packages/coding-agent/src/config/config-file.ts) 115～159、187～194 |
| 既有 legacy JSON 风险 | 两种 YAML 均不存在时，loader 可将同 stem 的既有 `models.json` 迁移为 `.yml`；存在 `.yaml` 时不走该迁移。证据：同一 `config-file.ts` 173～181。直接新建 `.yml` 会遮蔽 legacy 配置；本任务不扩展为任意 JSON 迁移。遇到 legacy-only 配置且没有安全支持的编辑方式，按 CA-22、CA-28～CA-30 保留并脱敏失败，而不是丢失旧 provider 后报告成功 |
| 隔离程序路径观测 | 主负责人报告已在 make_tmp 创建的 `.tmp/codeagent` 中运行实际 `omp config path`，覆盖 root / agent override / profile / fallback，四项均 exit 0 且没有生产配置写入；这仅证明对应 OMP 路径行为，不证明 CodeAgent 后台、同步或 GUI 验收 |

完整路径算法及后续执行证据由非合同的实现/验证说明维护；不能将本节事实或局部路径 smoke 冒充第 4 节功能矩阵通过。

### 3.2 目标 YAML 语义示例（初始默认模型，不是完整文件模板）

以下仅表示 CA-24～CA-26 的目标语义。`<解密后的 token>` 是说明占位，不是真实凭据、不允许作为实现 fallback；实际文件必须按 CA-27～CA-33 局部无损修改，不能用此片段覆盖现有文件。用户编辑模型后以持久模型列表替代本例。

```yaml
providers:
  codeagent:
    baseUrl: https://snapengine.cida.cce.prod-szv-y.dragon.tools.huawei.com/api/v2
    api: openai-completions
    auth: none
    headers:
      x-auth-token: "<解密后的 token>"
    disableStrictTools: true
    models:
      - id: DeepSeek-V4-Flash-0731-CodeAgent
        name: DeepSeek-V4-Flash-0731-CodeAgent
        reasoning: true
        supportsTools: true
        contextWindow: 200000
        maxTokens: 32768
```

## 4 验收矩阵（计划，不是执行结果）

| 编号 | 层级 / 隔离输入 | 必须观察并断言的结果 | 对应行为 | 当前状态 |
|---|---|---|---|---|
| CA-A01 | 确认记录 / 文档边界 | 明确覆盖全部 CA-01～CA-42；未确认前无产品修改，不借文档登记推定批准 | CA-01、CA-41～CA-42 | 用户已明确确认全部条目并实施 |
| CA-A02 | GUI 装配与真实服务入口的非桌面验证 | 独立注册及真实页面；异步启动/连接不阻塞；独立入口不启动 GUI、不提权、不启动 HTTP 或模型代理 | CA-02～CA-07、CA-09 | NOT RUN |
| CA-A03 | 真实进程 / 控制管道，重复及并发启动 | 同用户会话只有一个后台，状态来自该实例；跨用户/登录会话不借控制入口连接原实例 | CA-04～CA-06、CA-15 | NOT RUN |
| CA-A04 | GUI 生命周期非桌面 smoke / 真实后台 | 关闭及终止 GUI 后隔离 YAML 继续同步；重开连接相同实例；后台重启恢复设置，不新增登录自启 | CA-07～CA-10、CA-38 | NOT RUN |
| CA-A05 | 全新隔离 SQLite 与新后台 | 首次间隔 60 秒、完整默认模型正确；用户编辑多个模型及间隔后重启仍一致，不重置为默认 | CA-10～CA-12、CA-26、CA-38 | NOT RUN |
| CA-A06 | 异步保存 → SQLite → 通知 → 真实后台 → 隔离 YAML | 保存即时持久化并通知应用/同步；保存成功与同步结果分别报告，失败不虚报成功 | CA-13～CA-15、CA-38 | NOT RUN |
| CA-A07 | Windows 合成 DPAPI 凭据 | 固定 JSON 字段经 base64 / CurrentUser 无 entropy / 严格 UTF-8 得到合成 token；没有外部解密包装 | CA-03、CA-16～CA-18、CA-36 | NOT RUN |
| CA-A08 | 缺字段 / 错 base64 / 不可解密 / 非法 UTF-8 / 两轮 token 变化 | 错误脱敏、原件不变；有效 token 变化下一轮更新，不拿旧 token 冒报成功 | CA-17～CA-19、CA-32～CA-33、CA-36 | NOT RUN |
| CA-A09 | 源码 oracle 与隔离默认 / 多 profile / 新增 profile | 每轮覆盖真实现存范围；环境变量路径与 loader 源码一致；不读生产配置，不创建虚构 profile | CA-20～CA-22、CA-35、CA-37 | NOT RUN |
| CA-A10 | 同目录两种 models 扩展名及缺文件组合 | 严格按源码选择实际生效文件；不产生遮蔽文件，未选中的原有文件字节与 mtime 不变 | CA-21～CA-22、CA-27、CA-37 | NOT RUN |
| CA-A11 | 含其他 provider、额外 header 与 CodeAgent 非目标字段的文件 | 目标 baseUrl / api / auth / header / disableStrictTools / models 正确；非目标字段与字节原样保留 | CA-23～CA-26、CA-28、CA-39 | NOT RUN |
| CA-A12 | 等价目标语义、不同既有表示及重复轮询 | 报告未变化，不写入、不替换；全文件字节与 mtime 不变 | CA-27、CA-29、CA-39 | NOT RUN |
| CA-A13 | token / 多模型字段变化，含目标内与目标外注释、多种原有换行 / 编码 | 仅必要目标值变化；全部注释与非目标字节保持，编码和换行不被全局规范化 | CA-25～CA-29、CA-39 | NOT RUN |
| CA-A14 | 歧义 / 无法安全编辑的 YAML | 脱敏失败、原件字节与 mtime 不变；不整份序列化、不静默丢注释或缩为简单 YAML 成功 | CA-28～CA-30、CA-39 | NOT RUN |
| CA-A15 | 读取、写入、flush、Windows 原子替换失败及成功路径 | 同目录临时文件；成功完成 flush 与原子落位；失败原件完整且临时文件清理，无额外明文副本 | CA-31～CA-33、CA-39 | NOT RUN |
| CA-A16 | 合成敏感标记，错误 / 状态 / 日志 / 控制台 / SQLite 检查 | 除指定目标 header 与必需替换临时文件外不存在 token 或其他秘密；状态不回传 YAML 或凭据 | CA-15～CA-18、CA-30、CA-33 | NOT RUN |
| CA-A17 | 静态调用边界及非桌面运行观测 | 无出站请求，无 HTTP/model proxy，无连通性/真实模型调用；常驻与单实例不影响其他工具 | CA-02、CA-09、CA-34、CA-40 | NOT RUN |
| CA-A18 | 验证工作区与交付清单 | 所有合成测试/输出在 make_tmp 创建的 `.tmp`；定向授权范围明确；产品/可信基分离，注册与校验和同步；必要基线按可信基独立提交、不削弱既有断言 | CA-35～CA-42 | NOT RUN |

每项执行后另行记录入口或命令、Windows 环境、合成/真实边界、退出码及可观察结果，分别报告 PASS / FAIL / NOT RUN / UNVERIFIED；未运行、局部桩与“无崩溃”不计为完成。不存在的目标测试名或尚未实现入口不在本文伪装为可运行命令。真实桌面、完整门、真实凭据及真实供应商调用不因该矩阵而获得授权。

## 5 实际状态与未核实项（不属于行为批准记录）

| 类别 | 当前实际状态 |
|---|---|
| 行为确认 | 用户通过一次封闭选择明确确认全部 CA-01～CA-42 并实施 |
| 文档范围 | 本次仅新增本文，并登记需求索引、AGENTS 权威清单与静态需求注册表 |
| 产品实现 | 本文不证明任何 CodeAgent 产品实现；本次文档工作不改产品代码 |
| 路径 / loader 证据 | 专门研究已核实匹配 release 提交与第 3.1 节列出的源码事实；主负责人报告的四项隔离 OMP 路径 smoke 与尚未执行的 CodeAgent 验收分开 |
| 功能与质量验证 | 本次文档任务不运行测试、构建、静态检查、lint、格式化或测试门；第 4 节全为验收计划 |
| 提交 / 发布 / 校验和 | 用户本次已授权主负责人 Git 提交与推送；本次文档代理不执行提交、推送或发布。该授权不代替行为确认 |

全部编号行为已经实际确认；实现须继续按匹配源码落实路径及无损编辑，不把源码研究或提交授权当作功能验收，也不把尚未验证的功能降格为后续工作而提前宣称本工具完成。
