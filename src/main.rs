//! BORUIX PID 1：首个用户态进程（init）。
//!
//! 依赖 libsys 的薄封装调用 syscall。作为 M4.4 的真实用户程序，验证
//! 「Rust no_std 用户程序 → 编译成 ELF → 内核加载运行」完整链路。
//!
//! 入口约定：libsys 提供 `_start`，调用本文件导出的 `user_main`，
//! 其返回值作为进程退出码。

#![no_std]
#![no_main]

use libsys::{brk, info, waitpid_any, write, yield_now, STDOUT};

/// 把无符号整数格式化为十六进制字符串（写入固定缓冲），返回有效切片。
///
/// 缓冲布局：`buf[0..2] = "0x"`，`buf[2..18]` 为 16 个 hex 位（高位在前）。
/// 去掉前导零，至少保留一位。
fn hex_u64(v: u64, buf: &mut [u8; 18]) -> &[u8] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = (v >> ((15 - i) * 4)) & 0xF;
        buf[2 + i] = HEX[nibble as usize];
    }
    // 去掉前导零（buf[2..17] 可跳过），至少保留 buf[17] 最低位。
    let mut start = 2usize;
    while start < 17 && buf[start] == b'0' {
        start += 1;
    }
    &buf[..(20 - start)]
}

/// 输出一行 `label = value`（十六进制）。
fn print_hex(label: &[u8], v: u64) {
    let mut buf = [0u8; 18];
    let s = hex_u64(v, &mut buf);
    let _ = write(STDOUT, label);
    let _ = write(STDOUT, b" = ");
    let _ = write(STDOUT, s);
    let _ = write(STDOUT, b"\n");
}

/// init 主流程：打印信息、查询内核版本与堆断点、退出。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(_argc: isize, _argv: *const *const u8) -> i32 {
    // 1. 欢迎信息（验证 write）。
    let _ = write(STDOUT, b"[init] Hello from real userspace (Rust + libsys)!\n");

    // 2. 查询内核版本号（验证 info）。
    match info(libsys::nr::INFO_VERSION) {
        Ok(ver) => print_hex(b"[init] kernel version", ver),
        Err(_) => {
            let _ = write(STDOUT, b"[init] info(version) failed\n");
        }
    }

    // 3. 查询堆断点（验证 brk）。
    match brk(0) {
        Ok(b) => print_hex(b"[init] heap break", b),
        Err(_) => {
            let _ = write(STDOUT, b"[init] brk(0) failed\n");
        }
    }

    // 4. 主动让出 CPU（验证 yield 原语；单进程下无可让出，立即返回）。
    match yield_now() {
        Ok(()) => {
            let _ = write(STDOUT, b"[init] yielded cpu\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] yield failed\n");
        }
    }

    // 4.1 验证用户态 VFS 系统调用（M6.2: open/write/read/seek/readdir/mkdir/read_to_end）。
    let _ = write(STDOUT, b"[init] testing userspace VFS syscalls...\n");
    let _ = libsys::mkdir("/config/init_test", libsys::Permissions::all());
    match libsys::open(
        "/config/init_test/welcome.txt",
        libsys::OpenFlags::CREATE_OR_TRUNCATE,
        libsys::Permissions::all(),
    ) {
        Ok(fd) => {
            let _ = libsys::write(fd, b"BORUIX userspace VFS syscalls OK!");
            let _ = libsys::close(fd);
            match libsys::read_to_end("/config/init_test/welcome.txt") {
                Ok(bytes) => {
                    let _ = write(STDOUT, b"[init] read_to_end: ");
                    let _ = write(STDOUT, &bytes);
                    let _ = write(STDOUT, b"\n");
                }
                Err(_) => {
                    let _ = write(STDOUT, b"[init] read_to_end failed\n");
                }
            }
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] open failed\n");
        }
    }

    // 4.2 验证用户态 JSON 第一公民与特殊 VFS（M6.3: /system/cpu, /processes/list）。
    let _ = write(STDOUT, b"[init] reading /system/cpu JSON...\n");
    if let Ok(cpu_bytes) = libsys::read_to_end("/system/cpu") {
        let _ = write(STDOUT, b"[init] /system/cpu: ");
        let _ = write(STDOUT, &cpu_bytes);
    }
    let _ = write(STDOUT, b"[init] reading /processes/list JSON...\n");
    if let Ok(proc_bytes) = libsys::read_to_end("/processes/list") {
        let _ = write(STDOUT, b"[init] /processes/list: ");
        let _ = write(STDOUT, &proc_bytes);
    }

    // 5. init 进入 supervisor 循环：拉起 shell → 等其退出 → 重生。
    //    类 SysV 登录循环语义，PID 1 永不退出。
    //    也负责收尸被过继给 init 的孤儿进程，并区分日志。
    let _ = write(STDOUT, b"[init] entering supervisor loop\n");
    let mut shell_pid = 0u64;
    loop {
        match libsys::exec_path("/programs/shell.elf", &[]) {
            Ok(pid) => {
                shell_pid = pid;
                let mut buf = [0u8; 8];
                let _ = write(STDOUT, b"[init] shell started (pid ");
                let _ = write(STDOUT, dec_u64(pid, &mut buf));
                let _ = write(STDOUT, b")\n");
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] exec_path(shell.elf) failed, retrying...\n");
                // 启动失败时短眠再试（避免忙转），走 TASK_WAIT(0, 500ms)
                let _ = libsys::sleep(500_000_000);
                continue;
            }
        }
        // 等任意子进程退出（shell 或被过继给 init 的孤儿）。
        match waitpid_any() {
            Ok(code) => {
                // 检查 shell 是否还活着：读 /processes/{shell_pid}/status。
                // 若文件可读 → shell 还在，退出的是孤儿；
                // 若 NotFound → shell 没了，需要重生。
                let mut path_buf = [0u8; 32];
                let prefix = b"/processes/";
                let suffix = b"/status";
                path_buf[..prefix.len()].copy_from_slice(prefix);
                let mut pid_buf = [0u8; 8];
                let pid_str = dec_u64(shell_pid, &mut pid_buf);
                let start = prefix.len();
                path_buf[start..start + pid_str.len()].copy_from_slice(pid_str);
                let end = start + pid_str.len();
                path_buf[end..end + suffix.len()].copy_from_slice(suffix);
                let path = core::str::from_utf8(&path_buf[..end + suffix.len()])
                    .unwrap_or("/processes/list");
                if libsys::read_to_end(path).is_ok() {
                    // shell 仍在运行 → 退出的是被过继给 init 的孤儿。
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] reaped orphan (code ");
                    let _ = write(STDOUT, dec_u64(code, &mut buf));
                    let _ = write(STDOUT, b"), continuing\n");
                } else {
                    // shell 已退出 → 需要重生。
                    let mut buf = [0u8; 8];
                    let _ = write(STDOUT, b"[init] shell exited (code ");
                    let _ = write(STDOUT, dec_u64(code, &mut buf));
                    let _ = write(STDOUT, b"), respawning\n");
                }
            }
            Err(_) => {
                let _ = write(STDOUT, b"[init] waitpid_any() failed, retrying\n");
            }
        }
    }
}

/// 把无符号整数格式化为十进制字节，写入 `buf`，返回有效长度。
fn dec_u64(v: u64, buf: &mut [u8; 8]) -> &[u8] {
    let mut tmp = [0u8; 20];
    let mut n = v;
    let mut i = 0;
    if n == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut j = 0;
    while i > 0 {
        i -= 1;
        buf[j] = tmp[i];
        j += 1;
    }
    &buf[..j]
}
