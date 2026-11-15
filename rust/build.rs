// Slint 的 .slint 文件在编译期被编译成 Rust 代码（不是运行时解析），
// 所以改 UI 要重新编译——换来的是零运行时解析开销和编译期 UI 检查。
fn main() {
    slint_build::compile("src/bar.slint").expect("Slint 编译失败");

    // exe 图标：资源管理器/任务栏里显示柴犬（icon.ico 多尺寸，脚本生成）。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icon.ico");
        res.compile().expect("嵌入 icon.ico 失败");
    }
}
