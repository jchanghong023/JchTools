# rspolib

[![crates.io](https://img.shields.io/crates/v/rspolib?logo=rust)](https://crates.io/crates/rspolib) [![PyPI](https://img.shields.io/pypi/v/rspolib?logo=pypi&logoColor=white)](https://pypi.org/project/rspolib) [![docs.rs](https://img.shields.io/docsrs/rspolib?logo=docs.rs)](https://docs.rs/rspolib) [![Bindings docs](https://img.shields.io/badge/bindings-docs-blue?logo=python&logoColor=white)](https://github.com/mondeja/rspolib/blob/master/python/REFERENCE.md)

Port to Rust of the Python library [polib].

## Install

```bash
cargo add rspolib
```

## Usage

```rust
use rspolib::{pofile, prelude::*};

let po = pofile("./tests-data/flags.po").unwrap();

for entry in &po.entries {
    println!("{}", entry.msgid);
}

po.save("./file.po");
```

See the documentation at [docs.rs/rspolib](https://docs.rs/rspolib)

## JchTools 本地修正

- MO 上下文按 `msgctxt + EOT + msgid` 解码，保持普通/复数消息的标识、上下文查找及序列化往返。
- 从借用的 `FileOptions` 创建文件时保留二进制内容；MO 路径打开失败返回现有 `IOError`，不再因缺少字节内容而 panic。
- PO 普通、续行、previous 和复数字段在切片前验证引号；非法状态转换返回现有 `SyntaxError`，不对畸形输入 unwrap。
- 公开入口回归位于 `tests/parser_invariants.rs`，已登记为独立 Cargo 测试目标，覆盖上述负例和有效 Unicode、大小端 MO 往返。


## Python bindings

[![Python versions](https://img.shields.io/pypi/pyversions/rspolib?logo=python&logoColor=white)](https://pypi.org/project/rspolib/#files)

- [Quickstart](https://github.com/mondeja/rspolib/tree/master/python#readme)
- [Reference](https://github.com/mondeja/rspolib/blob/master/python/REFERENCE.md)

### Usage

```python
import polib
import rspolib

rspo = rspolib.pofile(f"{tests_dir}/django-complete.po")
pypo = polib.pofile(f"{tests_dir}/django-complete.po")
```

[polib]: https://github.com/izimobil/polib
