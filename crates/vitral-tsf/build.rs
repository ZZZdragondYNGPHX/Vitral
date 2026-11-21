fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .flag_if_supported("/utf-8")
        .include("include")
        .file("src/panel.cpp")
        .compile("vitral_panel");
    for lib in [
        "ole32", "advapi32", "user32", "gdi32", "d2d1", "dwrite", "dcomp", "dxgi", "d3d11",
        "dwmapi", "uuid",
    ] {
        println!("cargo:rustc-link-lib={lib}");
    }
    println!("cargo:rerun-if-changed=src/panel.cpp");
    println!("cargo:rerun-if-changed=include/vitral_tokens.hpp");
}
