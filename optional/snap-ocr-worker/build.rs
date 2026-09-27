// 截图 OCR worker 的 Slint 编译（结果窗，O-21）。
// 只编译 ui/result.slint；缺字体不影响编译（字体为运行期资产，O-05/O-09）。
fn main() {
    std::thread::Builder::new()
        .name("slint-compiler".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| slint_build::compile("ui/result.slint"))
        .unwrap_or_else(|error| panic!("无法启动 Slint 编译线程：{error}"))
        .join()
        .unwrap_or_else(|_| panic!("Slint 编译线程异常退出"))
        .unwrap_or_else(|error| panic!("result.slint 编译失败：{error}"));
}
