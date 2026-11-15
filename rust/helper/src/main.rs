#[path = "../../src/data.rs"]
mod data;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !data::handle_cli(&args) {
        eprintln!("用法：zcode-token-data --data-json [session_id] | --data-serve");
        std::process::exit(2);
    }
}
