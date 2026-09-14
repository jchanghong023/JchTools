# Windows 发布验收清单

下面是待执行验收，不是测试已通过的声明。所有破坏性场景使用专门的可丢弃测试目录或测试卷。

状态标记（用于对抗「代码与测试都由 AI 代理产出」的自证偏差）：

- **自动化**：有常驻测试守护，随 `cargo test` / CI / `acceptance.ps1` 运行；括号内是证据锚点（测试名 / 脚本）。
- **半自动**：有脚本入口但需人工触发与观察（`scripts/acceptance.ps1 -WithEngine/-WithGuiSmoke/-WithPackage`、`scripts/gui_smoke.py`）。
- **需人工**：依赖真实硬件、网络、数据或肉眼判断；**AI 代理不得宣称通过**，只能由人工执行后在本文件登记结果。
- **未执行**：尚无任何执行记录。

## 构建与界面

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| B1 | Windows x64 Rust stable/MSVC 全量编译；默认核心测试 + 显式真实引擎测试；PowerShell 脚本语法解析 | 自动化 | `.github/workflows/check.yml` windows job、`package-windows.ps1`、`acceptance.ps1 -WithEngine` |
| B2 | 启动/关闭、选择目录→解压与分析→计划生成→确认执行→整理结束 全链路实际操作 | 半自动 | `gui_smoke.py` S1–S3（UIA 驱动）+ `tests/gui_flow.rs`（无头确认流） |
| B3 | 搜索、侧栏、规则切换、原生目录对话框、冲突逐项与「应用到全部」、计划分页勾选、日志导出 | 需人工 | 2026-09-12 曾用 UIA 实测（docs/VALIDATION.md §4）；无常驻自动化 |
| B4 | 100%、150%、200% DPI、中文、亮暗主题、高对比度、远程桌面、软件渲染 | 需人工 | 150% 缩放与亮暗主题已实测；其余未验证 |
| B5 | 冷启动与热启动分别计时；不得把进程创建时间冒充首个可交互窗口时间 | 需人工 | 未验证 |

## 文件正确性

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| F1 | ZIP、7Z 固实包、TAR、TAR.GZ、BZ2、XZ、ZST、tgz 简写 | 自动化 | `zip_extracts_before_generating_dedup_plan`、`solid_7z_is_decoded_in_one_pass`、`bzip2_wrapped_tar_extracts_through_the_named_intermediate_tar`、`xz_single_stream_names_member_after_the_archive`、`zstd_stream_archive_extracts_with_content`、`tgz_shorthand_restores_the_tar_suffix`（真实引擎用例） |
| F2 | RAR4 / RAR5 | 需人工 | 无真实引擎用例；需含 RAR 的数据集 |
| F3 | 嵌套包递归、超深嵌套保留未处理包 | 自动化 | `nested_archives_are_processed_recursively`、`nested_archive_depth_limit_retains_unprocessed_package` |
| F4 | 重复条目（两份保留）、乱码文件名、空包/空目录 | 自动化 | `zip_with_duplicate_entries_extracts_both_via_auto_rename`、`gbk_filename_zip_extracts_without_data_loss`、`empty_zip_archive_extracts_cleanly`、`empty_7z_with_only_a_directory_entry_restores_the_directory` |
| F5 | 加密、损坏包、路径逃逸、符号链接 | 自动化 | `encrypted_archive_is_not_silently_deleted`、`corrupt_archive_is_kept_and_logged`、`unsafe_paths_rejected`、`symlink_not_followed_or_deleted` |
| F6 | CRC 错误变体、截断包、分卷缺失 | 需人工 | 未执行；需专门构造的坏包 |
| F7 | 冲突策略：覆盖禁止/跳过/最新/两份保留 | 自动化 | `skipped_conflict_always_preserves_original_archive`、`rename_never_overwrites`、`directory_scoped_conflicts_do_not_cross_directories` |
| F8 | 任何未完整解压或部分跳过的原包必须留下 | 自动化 | `skipped_conflict_always_preserves_original_archive`；「分卷源文件不被 Hash/版本清理删除」未验证 |
| F9 | 去重开关组合；相同大小不同中间字节；mtime 相等；真实重复不同名；同名不同内容；已有硬链接 | 自动化 | `same_prehash_different_middle_is_not_duplicate`、`deterministic_keeper_ties`、`same_name_equal_size_version_keeps_latest`、`same_name_different_size_version_keeps_newest`、`existing_hardlinks_not_counted_twice`、`hardlink_mode_preserves_aliases` |
| F10 | 计划生成后源文件、保留文件或目标路径被外部进程改变 | 自动化 | `changed_source_after_plan_is_skipped`、`changed_keeper_after_plan_prevents_delete`、`metadata_change_invalidates_snapshot` |
| F11 | 取消删除项导致目标未腾空时，后续移动安全失败而不是覆盖 | 自动化 | `rename_never_overwrites`、`unselecting_action_preserves_file` |
| F12 | 重复执行相同分类规则不出现分类目录套分类目录 | 自动化 | `classification_preserves_paths_and_is_idempotent`、`date_classification_is_idempotent`、`kept_source_reextract_does_not_recreate_classified_duplicate` |

## Windows 删除与文件系统

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| W1 | 本地 NTFS 正常回收（注入 mock 验证语义；真实回收站实操） | 自动化 + 半自动 | `successful_recycle_not_permanent`、`recycle_error_after_move_with_verified_bin_counts_as_recycled`（Windows 门禁）；真实回收站 2026-09-11 CLI 实测 |
| W2 | 限制回收站容量后回收大文件、禁用目标卷回收站、USB/不支持回收的位置、UNC/映射盘 | 需人工 | 未验证 |
| W3 | 回收失败降级开关分别开启与关闭 | 自动化 | `recycle_failure_without_permission_keeps_file`、`recycle_failure_with_permission_deletes` |
| W4 | Shell 用户取消与应用取消不得触发永久删除 | 自动化 | `user_cancel_never_falls_back_to_delete`、`keep_never_calls_recycler` |
| W5 | 日志明确区分回收与永久删除；无回收站计数时如实记为未验证 | 自动化 | `recycle_without_bin_counts_as_unverified` |
| W6 | 只读、共享占用、ACL 拒绝、Unicode 与长路径、reparse/junction、文件/目录重名、大小写冲突、磁盘空间不足 | 部分自动化 | `reserve_target_is_case_insensitive_unique`、`relative_tar_and_unicode_paths_accepted`、`windows_reserved_names_rejected`；其余（占用/ACL/长路径实测/磁盘不足）需人工，不要在真实生产网络共享上首次验证降级逻辑 |

## 压力与中断

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| S1 | 百万级目录项和数 TB 真实数据；单个数 TB 文件、大固实包、大量小文件 | 需人工 | 稀疏文件不能替代真实 TB IO 测试，也不能据此宣称真实性能 |
| S2 | 记录首屏、扫描/预哈希/完整 Hash/执行阶段时间、峰值 RSS、引擎内存、DB 与 staging 峰值、磁盘吞吐、取消响应、UI 卡顿 | 需人工 | 5004 文件 / 521 MiB 的中档基准见 docs/VALIDATION.md §1；TB 级未做 |
| S3 | 正常取消、暂停、关闭窗口、子进程失败 | 自动化 | `user_cancelled_analysis_records_cancelled_status`、`cancel_while_paused_makes_checkpoint_fail`、`run_idle_timeout_kills_silent_child`、`tests/gui_flow.rs` 关闭确认 |
| S4 | 应用强杀、磁盘断开 | 需人工 | 未执行 |
| S5 | 不自动重放历史删除；不清空别的 UUID 工作区；任务互斥 | 部分自动化 | `finished_plan_cannot_be_replayed`、`prepare_and_apply_share_exclusive_state_lock`、`task_lock_prevents_second_task`；强杀后的工作区隔离需人工 |
| S6 | 源包删除后已完成输出应存在；损坏/失败原包应保留 | 自动化 | `successful_source_can_be_deleted`、`corrupt_archive_is_kept_and_logged` |
| S7 | 异常残留工作区供人工检查；不把日志当成回滚机制 | 需人工 | 未执行 |

## 代理工具

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| P1 | 只读检测环境变量、系统代理、常见 VPN/代理进程与虚拟网卡；提供可复制设置命令；全程不自动修改系统代理 | 自动化 | `src/proxy.rs` 单元测试；真实系统差异需人工抽查 |
| P2 | 网络测试仅在用户点击时对固定目标站点发起 Windows 本机/WSL2 TCP 探测；不上传数据、不自动发起、不执行任意命令 | 半自动 | `src/nettest.rs` 单元测试（含 wsl.exe 非零退出按失败处理）；真实网络目标需人工点击验证 |
| P3 | 无 WSL 发行版时给出明确提示而不是失败崩溃 | 自动化 | `src/nettest.rs` |
| P4 | 整理引擎不联网这一边界不受代理工具影响 | 自动化 | 引擎/CLI 无网络依赖路径由核心测试覆盖 |

## 发布内容

| 编号 | 验收项 | 状态 | 证据 / 备注 |
|---|---|---|---|
| R1 | 独立干净 Windows 测试机（无安装版 7-Zip、无 Rust、无相关 PATH）运行完整便携包 | 需人工 | 未执行 |
| R2 | 篡改/缺失引擎触发拒绝；`7z.exe` 与 `7z.dll` 版本一致 | 自动化 | `src/engine_bundle.rs`（sha256 校验）、`fetch-7zip.ps1`（上游 SHA-256 清单）、`package-windows.ps1` fail-closed 检查 |
| R3 | 许可证、Slint 署名、7-Zip 对应源码、Cargo.lock 和实际 BUILD-INFO 保留 | 自动化 | `package-windows.ps1`（依赖图许可收集 + 缺失即中止）；内容抽查需人工 |
| R4 | 发布包不含 `7z.exe`/`7z.dll`，但保留 `licenses/`、`NOTICE.txt` 与上游源码压缩包 | 自动化 | `package-windows.ps1` 泄漏检查 |
| R5 | 不得把源码环境的静态检查记录说成 Windows 运行验收 | 规则 | AGENTS.md 3.2（NOT RUN 不得报告为通过；静态检查边界见 `static_check.py` 输出） |
