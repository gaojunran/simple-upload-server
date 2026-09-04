fn main() {
    if let Err(e) = upload_server::main_entry() {
        eprintln!("upload-server 启动失败: {e}");
        std::process::exit(1);
    }
}