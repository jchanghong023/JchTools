/// Only real, compiled-in tools are registered. New tools implement their own view/engine;
/// this registry does not execute arbitrary plugin code or create empty categories.
#[derive(Clone, Debug)]
pub struct ToolDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub category: &'static str,
    pub summary: &'static str,
}
pub fn tools() -> &'static [ToolDescriptor] {
    &[ToolDescriptor { id: "directory-organizer", name: "目录整理", category: "文件",
        summary: "递归解压 · 内容去重 · 冲突处理 · 分类清理" }]
}
