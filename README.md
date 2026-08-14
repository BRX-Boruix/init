# BORUIX init

PID 1 进程：内核启动后拉起的第一个用户态进程。

## 职责
- 初始化用户态运行时环境
- 启动/编排用户态服务（类似 `systemd`/经典 `init` 的服务编排职责）
- 作为可替换组件，可被其他 init 实现替代

## 内容规划
- `src/main.rs`       — 入口，接管 PID 1
- `src/services.rs`   — 服务拉起与监督
- 依赖 `libc`/`libsys`
