//! 性能基准（默认 `#[ignore]`，仅手动运行）：
//! `cargo test --release --test perf_probe -- --ignored --nocapture`
//! 规模经环境变量调整：`JT_PERF_GROUPS` / `JT_PERF_COPIES` / `JT_PERF_EMPTY_DIRS`。
//! 数据全部建在 tempfile（AGENTS §2），运行结束随 TempDir 销毁，不落仓库。
//! 基准只测耗时与计数，不断言具体秒数（机器相关）；计数断言用于确认
//! 两次运行走的是同一条行为路径，否则耗时对比没有意义。
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]
use jchtools::{config::Config, control::Context, engine};
use std::{fs, time::Instant};
use tempfile::TempDir;

fn env_or(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 启用 perf-tracing 特性时初始化性能日志（落在 state/perf-logs/），结束时打印 span 行。
/// 未启用特性时是空操作；这也让基准顺带充当 perf-tracing 端到端的冒烟验证。
#[cfg(feature = "perf-tracing")]
fn perf_init(state: &std::path::Path) -> Option<jchtools::perf::Guard> {
    jchtools::perf::init(state)
}
#[cfg(not(feature = "perf-tracing"))]
fn perf_init(state: &std::path::Path) -> Option<()> {
    let _ = state;
    None
}
fn perf_dump(state: &std::path::Path) {
    #[cfg(feature = "perf-tracing")]
    {
        let dir = state.join(jchtools::perf::LOG_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for line in text.lines() {
                if line.contains("time.busy") {
                    println!("[perf] {line}");
                }
            }
        }
    }
    #[cfg(not(feature = "perf-tracing"))]
    {
        let _ = state;
    }
}

/// 去重密集场景：G 组 × C 份同名副本分布在不同子目录，组间内容互不相同。
/// 覆盖 scan → hash（候选=全部）→ plan_dedup → apply 删除 (C-1)×G 个文件的热路径。
#[test]
#[ignore = "性能基准：生成与运行耗时数十秒，仅手动运行"]
fn organizer_dedup_probe() {
    let groups = env_or("JT_PERF_GROUPS", 5000);
    let copies = env_or("JT_PERF_COPIES", 8);
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    let gen = Instant::now();
    for g in 0..groups {
        // 组内容：组号驱动的确定性字节；大小在 4KiB–16KiB 间随组变化，避免所有组同尺寸。
        let size = 4096 + (g % 12) * 1024;
        let content: Vec<u8> = (0..size)
            .map(|i| {
                let group_byte = (g >> (8 * (i % 4))) as u8;
                group_byte.wrapping_add(i as u8 % 251)
            })
            .collect();
        for c in 0..copies {
            // 组号全量作目录层级（不取模），保证不同组绝不共享路径。
            let dir = root.join(format!("g{g:04}/copy{c:02}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("file.bin"), &content).unwrap();
        }
    }
    println!(
        "[dedup] 数据生成：{groups} 组 × {copies} 副本 = {} 文件，{:.2}s（不计入测量）",
        groups * copies,
        gen.elapsed().as_secs_f64()
    );
    let _perf_guard = perf_init(&state);
    let t0 = Instant::now();
    let prepared =
        engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    let t_prepare = t0.elapsed();
    let t1 = Instant::now();
    let done = engine::apply(&prepared.directory, Context::default()).unwrap();
    let t_apply = t1.elapsed();
    println!(
        "[dedup] prepare(分析+计划)：{:.3}s  apply(执行)：{:.3}s  total：{:.3}s",
        t_prepare.as_secs_f64(),
        t_apply.as_secs_f64(),
        (t_prepare + t_apply).as_secs_f64()
    );
    // 同名副本每组保留 1 份：删除不少于 (C-1)×G（腾空的 copy 目录还会被空目录清理
    // 计入 deleted）；精确值打印在 summary，两次对比运行必须一致否则耗时对比失效。
    let expected = (copies as u64 - 1) * groups as u64;
    assert!(
        done.summary.deleted >= expected,
        "删除计数 {} 应不少于副本淘汰数 {expected}；summary：{}",
        done.summary.deleted,
        done.summary.description()
    );
    println!(
        "[dedup] deleted={} moved={}",
        done.summary.deleted, done.summary.moved
    );
    perf_dump(&state);
}

/// 去重复用场景（C-13）：同一数据、同一状态目录连跑两次分析。
/// 第一次冷启动（计算全部哈希并写入跨运行缓存），第二次热启动（三要素
/// 未变的候选全部命中缓存，不重读内容）。规模环境变量与 dedup 探针同口径，
/// 另加 JT_PERF_FILE_KIB 固定单文件大小（KiB，默认 0 = 按 dedup 探针的
/// 4–16KiB 随组变化），用于大文件场景（如 4 组 × 4 副本 × 512MiB ≈ 8GiB）。
#[test]
#[ignore = "性能基准：生成与运行耗时数十秒，仅手动运行"]
fn organizer_dedup_reuse_probe() {
    let groups = env_or("JT_PERF_GROUPS", 5000);
    let copies = env_or("JT_PERF_COPIES", 8);
    let fixed_kib = env_or("JT_PERF_FILE_KIB", 0);
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    let gen = Instant::now();
    for g in 0..groups {
        let size = if fixed_kib > 0 {
            fixed_kib * 1024
        } else {
            4096 + (g % 12) * 1024
        };
        let content: Vec<u8> = (0..size)
            .map(|i| {
                let group_byte = (g >> (8 * (i % 4))) as u8;
                group_byte.wrapping_add(i as u8 % 251)
            })
            .collect();
        for c in 0..copies {
            let dir = root.join(format!("g{g:04}/copy{c:02}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("file.bin"), &content).unwrap();
        }
    }
    println!(
        "[reuse] 数据生成：{groups} 组 × {copies} 副本 = {} 文件（单文件 {}），{:.2}s（不计入测量）",
        groups * copies,
        if fixed_kib > 0 {
            format!("{fixed_kib}KiB")
        } else {
            "4–16KiB".into()
        },
        gen.elapsed().as_secs_f64()
    );
    let t0 = Instant::now();
    let cold = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    let t_cold = t0.elapsed();
    assert!(
        state.join("hash-cache.sqlite3").is_file(),
        "首轮分析应产出跨运行哈希缓存（C-13）"
    );
    let t1 = Instant::now();
    let warm = engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    let t_warm = t1.elapsed();
    // 两次走的是同一条计划路径，否则耗时对比没有意义。
    assert_eq!(
        cold.summary.planned_delete,
        warm.summary.planned_delete,
        "冷/热两轮的去重计划必须一致；summary：{} / {}",
        cold.summary.description(),
        warm.summary.description()
    );
    let expected = u64::try_from((copies - 1) * groups).unwrap();
    assert!(
        cold.summary.planned_delete >= expected,
        "删除计划 {} 应不少于副本淘汰数 {expected}",
        cold.summary.planned_delete
    );
    println!(
        "[reuse] cold(全量分析)：{:.3}s  warm(缓存复用)：{:.3}s  省 {:.1}%  planned_delete={}",
        t_cold.as_secs_f64(),
        t_warm.as_secs_f64(),
        (1.0 - t_warm.as_secs_f64() / t_cold.as_secs_f64()) * 100.0,
        cold.summary.planned_delete
    );
}

/// 空目录场景：N 个嵌套空目录，覆盖 plan_empty_dirs 物化临时表与 apply 自底向上删除。
#[test]
#[ignore = "性能基准：生成与运行耗时数十秒，仅手动运行"]
fn organizer_empty_dirs_probe() {
    let total = env_or("JT_PERF_EMPTY_DIRS", 12000);
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("data");
    let state = temp.path().join("state");
    let gen = Instant::now();
    // 每个编号按 8 进制数位分层，形成共享前缀的嵌套树；叶子全部为空目录。
    // 编号从 1 起：0 的数位为空会退化成 root 自身，不被扫描登记。
    for i in 1..=total {
        let mut path = root.clone();
        let mut n = i;
        let mut digits = Vec::new();
        while n > 0 {
            digits.push(n % 8);
            n /= 8;
        }
        digits.reverse();
        for d in digits {
            path.push(format!("d{d}"));
        }
        fs::create_dir_all(&path).unwrap();
    }
    println!(
        "[empty] 数据生成：{total} 个空目录，{:.2}s（不计入测量）",
        gen.elapsed().as_secs_f64()
    );
    let _perf_guard = perf_init(&state);
    let t0 = Instant::now();
    let prepared =
        engine::prepare_at(&root, Config::default(), Context::default(), &state).unwrap();
    let t_prepare = t0.elapsed();
    let t1 = Instant::now();
    let done = engine::apply(&prepared.directory, Context::default()).unwrap();
    let t_apply = t1.elapsed();
    println!(
        "[empty] prepare(分析+计划)：{:.3}s  apply(执行)：{:.3}s  total：{:.3}s",
        t_prepare.as_secs_f64(),
        t_apply.as_secs_f64(),
        (t_prepare + t_apply).as_secs_f64()
    );
    // 全部编号路径均为空目录：删除不少于 N（中间目录随叶子清空也被删除）。
    assert!(
        done.summary.deleted >= total as u64,
        "空目录删除计数 {} 应不少于 {total}；summary：{}",
        done.summary.deleted,
        done.summary.description()
    );
    println!(
        "[empty] deleted={} moved={}",
        done.summary.deleted, done.summary.moved
    );
    perf_dump(&state);
}
