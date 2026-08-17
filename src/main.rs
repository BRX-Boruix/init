//! BORUIX PID 1：首个用户态进程（init）。
//!
//! 依赖 libsys 的薄封装调用 syscall。作为 M4.4 的真实用户程序，验证
//! 「Rust no_std 用户程序 → 编译成 ELF → 内核加载运行」完整链路。
//!
//! 入口约定：libsys 提供 `_start`，调用本文件导出的 `user_main`，
//! 其返回值作为进程退出码。

#![no_std]
#![no_main]

use libsys::{brk, exit, info, write, STDOUT};

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

    // 4. 正常退出（验证 exit）。
    let _ = write(STDOUT, b"[init] exiting normally\n");
    exit(0)
}
