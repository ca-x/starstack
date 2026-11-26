fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    assert!(
        std::path::Path::new("web/dist/index.html").is_file(),
        "Build the embedded web UI first: cd web && npm ci && npm run build"
    );
}
