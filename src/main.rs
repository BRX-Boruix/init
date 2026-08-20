//! BORUIX PID 1：首个用户态进程（init）。
//!
//! 依赖 libsys 的薄封装调用 syscall。作为 M4.4 的真实用户程序，验证
//! 「Rust no_std 用户程序 → 编译成 ELF → 内核加载运行」完整链路。
//!
//! 入口约定：libsys 提供 `_start`，调用本文件导出的 `user_main`，
//! 其返回值作为进程退出码。

#![no_std]
#![no_main]

use libsys::{brk, exit, info, write, yield_now, STDOUT};

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

    // 5. init 经 exec 系统调用加载并运行独立编译的 shell.elf（PID 2）。
    //    不再编译期引用 shell crate，而是"运行 shell 程序"（ADR-003 纯 spawn）。
    let _ = write(STDOUT, b"[init] launching shell via exec\n");
    match libsys::exec(libsys::nr::PROG_SHELL, &[]) {
        Ok(pid) => {
            let _ = write(STDOUT, b"[init] shell started (pid ");
            let mut buf = [0u8; 8];
            let _ = write(STDOUT, dec_u64(pid, &mut buf));
            let _ = write(STDOUT, b")\n");
        }
        Err(_) => {
            let _ = write(STDOUT, b"[init] exec(shell) failed\n");
        }
    }

    // 6. init 完成引导职责，让出 CPU 并退出（shell 独立运行）。
    let _ = write(STDOUT, b"[init] init done, exiting\n");
    exit(0)
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
