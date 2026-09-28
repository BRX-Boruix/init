# init

BORUIX 的初始化进程：系统里第一个用户态进程，把系统从「内核已就绪」带到「可以使用」。

[English](README.en.md)

## 它做什么

- 按固定顺序拉起各守护进程与硬件驱动
- 常驻监督：守护进程退出后按原实例号重新拉起
- 为每个终端实例拉起终端守护进程与登录程序
- 处理运行期的新终端请求

## 终端实例

实例数量在构建期注入（环境变量 `BORUIX_CONSOLES_N`，默认 4，钳制在 1 到 256），与内核预创建的
数量同源同值——内核预创建几个，初始化进程就服务几个。

两种会话模式：

- **轮转**（默认）——同一时刻只有一块终端在服务，一个会话结束后切换到下一块
- **并行**——每块终端各自独立地同时提供服务

## 运行期新建终端

请求方在 `/system/console-requests/` 下创建以目标编号命名的文件；初始化进程巡检该目录：校验
编号、拉起该终端的守护与登录、登记、删除请求文件。没有为此新增系统调用——文件创建的原子性
天然提供了重复请求去重。

非法请求文件会被如实删除并留下记录，不静默忽略。

## 已知限制

- 请求文件名必须是 1 到 63 的纯数字；0 号是系统主控制台的别名，不可请求
- 巡检周期 200 毫秒，即新终端请求的最大响应延迟
- 音频消费者缺席时混音守护进程跳过启动并如实说明

## 构建

```bash
cargo build --release
```

## 文件结构

```
init/
├── Cargo.toml    # 包定义
├── build.rs      # 注入链接脚本与构建期选项
├── linker.ld     # 用户态段布局
└── src/
    └── main.rs   # 启动序列、监督循环与终端管理
```

## 相关项目

- [`shell`](https://github.com/BRX-Boruix/shell) —— 初始化完成后运行的用户 shell
- [`login`](https://github.com/BRX-Boruix/login) —— 登录认证程序
- [`consoled`](https://github.com/BRX-Boruix/consoled) —— 终端字节生产者
- [`openvt`](https://github.com/BRX-Boruix/openvt) —— 运行期申请新终端的用户入口
- [`libsys`](https://github.com/BRX-Boruix/libsys) —— 用户态系统调用封装

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。
