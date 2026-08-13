# Contributing

## 开发环境

- Windows 10/11
- Rust stable MSVC toolchain

## 验证

```powershell
cargo check
cargo test
cargo build --release
```

提交前请确保：

- 默认策略仍为只报告，不自动结束进程。
- 终止逻辑继续校验 PID 与创建时间。
- 不将命令行、令牌或个人数据写入仓库、状态文件或测试数据。
- 新增行为具有相应回归测试。

## Pull Request

PR 应说明问题、实现方式、测试结果和潜在安全影响。请保持改动小而可验证。

