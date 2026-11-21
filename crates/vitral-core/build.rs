use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=VITRAL_RIME_LIB_DIR");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let native_rime = env::var_os("CARGO_FEATURE_NATIVE_RIME").is_some();

    if target_os != "linux" || !native_rime {
        return;
    }

    if let Ok(lib_dir) = env::var("VITRAL_RIME_LIB_DIR") {
        if !lib_dir.trim().is_empty() {
            println!("cargo:rustc-link-search=native={lib_dir}");
        }
    }

    let probed = Command::new("pkg-config")
        .args(["--libs", "rime", "opencc"])
        .output();

    let mut emitted_rime = false;
    let mut emitted_opencc = false;

    if let Ok(out) = probed {
        if out.status.success() {
            let flags = String::from_utf8_lossy(&out.stdout);
            for tok in flags.split_whitespace() {
                if let Some(dir) = tok.strip_prefix("-L") {
                    if !dir.is_empty() {
                        println!("cargo:rustc-link-search=native={dir}");
                    }
                } else if let Some(lib) = tok.strip_prefix("-l") {
                    if !lib.is_empty() {
                        println!("cargo:rustc-link-lib={lib}");
                        if lib == "rime" {
                            emitted_rime = true;
                        } else if lib == "opencc" {
                            emitted_opencc = true;
                        }
                    }
                }
            }
        }
    }

    if !emitted_rime {
        println!("cargo:rustc-link-lib=rime");
    }
    if !emitted_opencc {
        println!("cargo:rustc-link-lib=opencc");
    }
}
