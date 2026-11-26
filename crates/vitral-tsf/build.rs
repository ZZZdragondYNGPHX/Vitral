fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    compile_icon_resource();
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

fn compile_icon_resource() {
    use std::{env, fs, path::PathBuf, process::Command};
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../../apps/desktop-tauri/src-tauri/icons/icon.ico").canonicalize().unwrap();
    let rc_file = out.join("vitral.rc");
    // Use a stable resource ID in the TIP itself, independently of the desktop host.
    fs::write(&rc_file, format!("101 ICON \"{}\"\n", icon.display().to_string().replace('\\', "/").trim_start_matches("//?/"))).unwrap();
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let resource = out.join(if msvc { "vitral.res" } else { "vitral-icons.o" });
    let mut command = if msvc {
        let compiler = cc::Build::new().get_compiler();
        let search = compiler.get_envs().find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| v.to_os_string()).or_else(|| env::var_os("PATH")).unwrap_or_default();
        let rc = env::var_os("RC").map(PathBuf::from).or_else(|| env::split_paths(&search)
            .flat_map(|p| [p.join("rc.exe"), p.join("llvm-rc"), p.join("llvm-rc.exe")])
            .find(|p| p.is_file())).expect("Windows SDK rc.exe or LLVM llvm-rc is required");
        let mut command = Command::new(rc);
        command.envs(compiler.get_envs()).arg("/nologo").arg("/fo").arg(&resource).arg(&rc_file);
        command
    } else {
        let mut command = Command::new(env::var_os("RC").unwrap_or_else(|| "windres".into()));
        command.arg(&rc_file).arg(&resource);
        command
    };
    assert!(command.status().expect("run Windows resource compiler").success(), "icon resource compilation failed");
    println!("cargo:rustc-link-arg={}", resource.display());
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-env-changed=RC");
}
