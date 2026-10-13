## JchTools 本地修正
- CamelCase 按首字符的 UTF-8 边界处理非 ASCII Rust 标识符，空名称或仅下划线字段的空转换结果不再触发切片 panic；保持既有 ASCII 大小写转换语义。

## v1.0.1 - March 18, 2019
- Add `LICENSE` file (#3)[https://github.com/TedDriggs/ident_case/issues/3]