//! Slint 源文件静态检查门禁（用户确认 2026-09-18）。
//! 现有拦截：构建期编译错误（slint-build）+ static_check 的 5 项结构检查
//! + acceptance 的运行期绑定环扫描；本门禁补上「编译警告只打印不拦截」的缺口。

use slint_interpreter::Compiler;

#[test]
fn app_slint_compiles_with_zero_diagnostics() {
    let mut compiler = Compiler::default();
    // 与 build.rs 保持同一风格配置，诊断口径一致
    compiler.set_style("fluent".into());
    let result = spin_on::spin_on(compiler.build_from_path("ui/app.slint"));
    let offenders: Vec<String> = result.diagnostics().map(|d| format!("{:?}", d)).collect();
    assert!(
        offenders.is_empty(),
        "ui/app.slint 存在编译诊断（警告同样视为失败）:\n{}",
        offenders.join("\n")
    );
    assert!(
        result.component("AppWindow").is_some(),
        "ui/app.slint 编译后必须仍能导出 AppWindow"
    );
}
