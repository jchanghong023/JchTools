#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! Real-engine tests; intentionally ignored without an explicit JCHTOOLS_TEST_7ZIP path.
//! package-windows.ps1 runs these after downloading and verifying the bundled engine.
//! 两工具独立 crate 后：解压用例走 recursive_extract::engine::extract_run_at（X 分区），
//! 涉及整理计划的用例先解压再 prepare/apply（C 分区）。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools_directory_organizer::engine;
use jchtools_file_core::config::*;
use jchtools_file_core::control::Context;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
struct ArchiveFixture {
    tmp: tempfile::TempDir,
    root: PathBuf,
    input: PathBuf,
    state: PathBuf,
    engine: PathBuf,
}
impl ArchiveFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let input = tmp.path().join("input");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&input).unwrap();
        let state = tmp.path().join("state");
        let engine = PathBuf::from(
            std::env::var_os("JCHTOOLS_TEST_7ZIP")
                .expect("Set JCHTOOLS_TEST_7ZIP to the full 7z.exe or 7zz absolute path"),
        );
        assert!(engine.is_absolute());
        Self {
            tmp,
            root,
            input,
            state,
            engine,
        }
    }
    fn archive(&self, path: &Path, format: &str) {
        let status = Command::new(&self.engine)
            .current_dir(&self.input)
            .args(["a", "-y", format])
            .arg(path)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
    }
    /// 用给定格式链与显式成员打包（例如 `-tbzip2` 压缩一个已有 tar，或 `-txz` 压单个文件）。
    fn pack(&self, archive: &Path, formats: &[&str], sources: &[&str]) {
        let mut args = vec!["a", "-y"];
        args.extend_from_slice(formats);
        let status = Command::new(&self.engine)
            .current_dir(&self.input)
            .args(args)
            .arg(archive)
            .args(sources)
            .status()
            .unwrap();
        assert!(status.success());
    }
    /// 生成编号式分卷组（`-v1k` + 不可压缩数据才会真的切出多卷），返回卷文件数（含 `.001`）。
    /// 成员为 6 个 1500 字节的伪随机文件，合计约 9 KiB，与单卷 1 KiB 明显不同——
    /// 「展开比例」类的断言据此区分「按单卷算分母」与「按实际卷集合合计算分母」。
    fn split_set(&self, archive: &Path) -> usize {
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut random_bytes = std::iter::repeat_with(move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            u8::try_from((seed >> 33) & 0xff).unwrap()
        });
        for i in 0..6 {
            let data: Vec<u8> = random_bytes.by_ref().take(1500).collect();
            fs::write(self.input.join(format!("f{i}.txt")), data).unwrap();
        }
        self.pack(archive, &["-tzip", "-v1k"], &["."]);
        let prefix = format!("{}.", archive.file_name().unwrap().to_string_lossy());
        fs::read_dir(archive.parent().unwrap())
            .unwrap()
            .map(|entry| entry.expect("读取分卷夹具目录项失败"))
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count()
    }
    /// 「递归解压」一段式运行（X-02）：解压 + 未完全解开的包隔离全部完成。
    fn run(&self, cfg: Config) -> jchtools_file_core::task::TaskResult {
        jchtools_recursive_extract::engine::extract_run_at(
            &self.root,
            cfg,
            Context::default(),
            &self.state,
            Some(&self.engine),
        )
        .unwrap()
    }
    fn apply(task: &jchtools_file_core::task::TaskResult) -> jchtools_file_core::task::TaskResult {
        engine::apply(&task.directory, Context::default()).unwrap()
    }
}
/// 解压用例的默认规则：X-05 之后成功原包一律永久删除、冲突一律只给新成员改名，
/// 处置类配置已整体移除（无删除选项可配），这里只关掉与本文件无关的容量限制（预留 0、展开比例不限）。
fn config() -> Config {
    Config {
        reserve_bytes: 0,
        max_ratio: 0,
        ..Config::default()
    }
}
/// 整理（第二工具）用例的规则：固定归类（C-05 恒开启）+ 全局永久删除。
fn organizer() -> Config {
    Config {
        global_delete: DeleteMode::Permanent,
        ..Config::default()
    }
}

// ===== 手工构造的特殊压缩包样本（重复条目 / GBK 文件名 / 空包 / ZST）=====
// 7-Zip 无法直接创建这些样本（同名条目、原始字节文件名、空包、zstd 帧），因此按
// PKWARE APPNOTE 与 RFC 8878 手工生成字节；全部为存储（未压缩）条目，无需压缩器。

/// 标准 CRC-32（poly 0xEDB8_8320，初值与输出取反）。
fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        table[i as usize] = c;
    }
    let mut crc = !0u32;
    for &b in bytes {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}
/// 生成只含"存储"条目的极简 ZIP（本地文件头 + 中央目录 + EOCD）。
/// 文件名按原始字节写入，不带 UTF-8 标志：可构造 GBK 字节名与同名重复条目。
fn stored_zip(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (name, data) in entries {
        let crc = crc32(data);
        let header_offset = offset;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes()); // 版本
        out.extend_from_slice(&0u16.to_le_bytes()); // 标志：无 UTF-8 位
        out.extend_from_slice(&0u16.to_le_bytes()); // 方法：存储
        out.extend_from_slice(&[0, 0, 0, 0]); // 时间/日期
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra 长度
        out.extend_from_slice(name);
        out.extend_from_slice(data);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes()); // 制作版本
        central.extend_from_slice(&20u16.to_le_bytes()); // 需要版本
        central.extend_from_slice(&0u16.to_le_bytes()); // 标志
        central.extend_from_slice(&0u16.to_le_bytes()); // 方法
        central.extend_from_slice(&[0, 0, 0, 0]); // 时间/日期
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        central.extend_from_slice(&[0; 8]); // extra 长度/注释长度/起始盘号/内部属性
        central.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
        central.extend_from_slice(&header_offset.to_le_bytes());
        central.extend_from_slice(name);
        offset += u32::try_from(30 + name.len() + data.len()).unwrap();
    }
    let central_offset = offset;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06"); // EOCD
    out.extend_from_slice(&[0, 0, 0, 0]); // 盘号
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&u32::try_from(central.len()).unwrap().to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // 注释长度
    out
}

// 测试按主题拆分为子模块；本文件保留共享夹具、辅助函数与模块声明。
#[path = "archive/collision_review.rs"]
mod collision_review;
#[path = "archive/extraction_flow.rs"]
mod extraction_flow;
#[path = "archive/handmade_zip.rs"]
mod handmade_zip;
#[path = "archive/semantics_regression.rs"]
mod semantics_regression;
#[path = "archive/volume_cleanup.rs"]
mod volume_cleanup;
#[path = "archive/windows_edge.rs"]
mod windows_edge;
