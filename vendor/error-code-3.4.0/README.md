# error-code

[![Crates.io](https://img.shields.io/crates/v/error-code.svg)](https://crates.io/crates/error-code)
[![Documentation](https://docs.rs/error-code/badge.svg)](https://docs.rs/crate/error-code/)
[![Rust](https://github.com/DoumanAsh/error-code/actions/workflows/rust.yml/badge.svg)](https://github.com/DoumanAsh/error-code/actions/workflows/rust.yml)

Error code library provides generic errno/winapi error wrapper

User can define own `Category` if you want to create new error wrapper.

## Features

- `std` - Enables `std::error::Error` implementation and conversion from `std::io::Error`

## Usage

```rust
use error_code::ErrorCode;

use std::fs::File;

File::open("non_existing");
println!("{}", ErrorCode::last_system());
```

## 本地修复

- Windows 下启用 `std` 后，`std::io::Error` 的原生 Win32/Winsock 错误码转换为系统错误类别，保留系统消息与 WouldBlock 判断；无原生错误码时仍使用既有的 POSIX `-1` 回退，其他平台行为不变。
