//! 用户态程序链接脚本注入。
//!
//! 通过 `CARGO_MANIFEST_DIR` 得到绝对路径，把 `linker.ld` 传给链接器，
//! 将 `.text` 等段定位到用户态地址（0x400000 起），`ENTRY(_start)`。
//!
//! `-no-pie`：强制生成 ET_EXEC（非 PIE）。rust-lld 默认生成 PIE（ET_DYN），
//! 而内核 ELF 加载器雏形只接受 ET_EXEC；裸机静态程序无需重定位，故关闭 PIE。

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{}/linker.ld", dir);
    println!("cargo:rustc-link-arg=-no-pie");
}
