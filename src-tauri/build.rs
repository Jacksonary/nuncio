fn main() {
    // 从 ../.env.local 读取 OAuth Client ID 等构建期变量（不存在则跳过），供 option_env! 使用
    let env_path = std::path::Path::new("../.env.local");
    println!("cargo:rerun-if-changed=../.env.local");
    for key in [
        "NUNCIO_GOOGLE_CLIENT_ID",
        "NUNCIO_GOOGLE_CLIENT_SECRET",
        "NUNCIO_MS_CLIENT_ID",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    if let Ok(content) = std::fs::read_to_string(env_path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if k.starts_with("NUNCIO_") && !v.is_empty() && std::env::var(k).is_err() {
                    println!("cargo:rustc-env={k}={v}");
                }
            }
        }
    }
    tauri_build::build()
}
