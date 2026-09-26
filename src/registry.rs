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
    &[
        ToolDescriptor {
            id: "recursive-extract",
            name: "递归解压",
            category: "文件",
            summary: "递归解开压缩包 · 成功后删除原包 · 冲突自动改名",
        },
        ToolDescriptor {
            id: "directory-organizer",
            name: "目录整理",
            category: "文件",
            summary: "内容去重 · 归类 · 清理（不解压）",
        },
        ToolDescriptor {
            id: "md-organizer",
            name: "MD 整理",
            category: "文档",
            summary: "合并 MD（标题下移一级）· 拆分 MD（UTF-8 安全 · 无损）",
        },
        ToolDescriptor {
            id: "markdown-converter",
            name: "转 Markdown",
            category: "文档",
            summary: "文档、图片与媒体转换为 Markdown · 支持离线处理",
        },
        ToolDescriptor {
            id: "git-tools",
            name: "Git 工具",
            category: "开发",
            summary: "逐文件提交并推送 · 自动重试 · 冲突即停",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::tools;

    // 覆盖 P-02
    #[test]
    fn registry_ids_unique_and_fields_nonempty() {
        let list = tools();
        assert!(!list.is_empty(), "注册表至少应包含一个真实工具");
        for (i, tool) in list.iter().enumerate() {
            assert!(!tool.id.is_empty(), "第 {i} 项 id 为空");
            assert!(!tool.name.is_empty(), "第 {i} 项 name 为空");
            assert!(!tool.category.is_empty(), "第 {i} 项 category 为空");
            assert!(!tool.summary.is_empty(), "第 {i} 项 summary 为空");
            assert_eq!(tool.id, tool.id.trim(), "id 不应含首尾空白：{:?}", tool.id);
            assert_eq!(
                tool.name,
                tool.name.trim(),
                "name 不应含首尾空白：{:?}",
                tool.name
            );
        }
        let ids: std::collections::HashSet<_> = list.iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), list.len(), "工具 id 必须全局唯一");
    }
}
