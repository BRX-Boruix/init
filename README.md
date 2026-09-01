# BORUIX init

PID 1 进程：内核启动后拉起的第一个用户态进程。

## 职责
- 初始化用户态运行时环境
- 启动/编排用户态服务（类似 `systemd`/经典 `init` 的服务编排职责）
- 作为可替换组件，可被其他 init 实现替代

## 当前状态（M4.4 雏形）

作为 M4.4「真实用户程序工具链」的验收程序：Rust `no_std` + `no_main`，依赖 `libsys`
薄封装，`user_main` 用 Rust 风格 `Result` 调 `write`/`info`/`brk`/`exit`。

编译产物 `target/x86_64-unknown-none/release/init` 由 SDK `build --test-m4.4` 复制到
内核源码目录，内核测试用 `include_bytes!` 嵌入并加载运行。

## 构建
```bash
cargo build --manifest-path init/Cargo.toml --target x86_64-unknown-none --release
```

- `linker.ld`：段页对齐 + 丢弃动态段（裸机静态程序）
- `build.rs`：注入链接脚本 + `-no-pie`（强制 ET_EXEC）

## 内容规划
- `src/main.rs` — 入口（导出 `user_main`，libsys 的 `_start` 调用它）
- `src/services.rs` — 服务拉起与监督（待实现）
- 依赖 `libsys` + `libc`

## libc 开机自检（libc_selftest）

init 启动时调用 `libc_selftest()`，验证「内核→libsys→libc→init」最小链路在开机即通：
逐项输出 `[init] libc: ... OK/FAIL`，末尾汇总 `[init] libc: selftest passed=N failed=M`。
覆盖：malloc/free 堆分配、strlen/strcmp、snprintf（整数+浮点）、strtol、time。
防御式——失败仅记录，不中断启动流程。
