//! 桌面端可执行入口。
//!
//! 真正的逻辑都在 lib.rs 的 run()，这里只做转发 ——
//! 这样同一份 lib 能同时服务桌面端和 Android/iOS。

fn main() {
    missav_lib::run();
}
