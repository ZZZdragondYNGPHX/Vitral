#[cfg(target_os = "linux")]
mod session;
use std::env;
use std::process::ExitCode;
use vitral_core::VitralCore;
use vitral_tokens::{validate_generated_parity, TOKENS};

const VITRAL_ABI_VERSION: u32 = 1;

fn print_usage() {
    eprintln!("Usage: vitral <health|tokens-verify|abi-version> or vitral --state-dir DIR COMMAND");
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print_usage();
        return ExitCode::from(2);
    };

    match cmd {
        #[cfg(target_os = "linux")]
        "--state-dir" | "--session-worker" => match session::run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        "health" => match VitralCore::init("{}") {
            Ok(core) => {
                let out = serde_json::json!({
                    "status": "ok",
                    "brand": TOKENS.brand.name,
                    "version": TOKENS.brand.version,
                    "abi_version": VITRAL_ABI_VERSION,
                    "pure_offline": core.is_pure_offline(),
                });
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("health check failed: {err}");
                ExitCode::from(1)
            }
        },
        "tokens-verify" => match validate_generated_parity() {
            Ok(()) => {
                let out = serde_json::json!({
                    "status": "ok",
                    "brand": TOKENS.brand.name,
                    "version": TOKENS.brand.version,
                    "toolbar_text_labels_allowed": TOKENS.typography.toolbar_text_labels_allowed,
                    "desktop_candidate_page_size": TOKENS.ergonomics.desktop_candidate_page_size,
                });
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(errs) => {
                for e in errs {
                    eprintln!("[Group {}] {}: {}", e.group, e.field, e.message);
                }
                ExitCode::from(1)
            }
        },
        "abi-version" => {
            println!("{VITRAL_ABI_VERSION}");
            ExitCode::SUCCESS
        }
        _ => {
            print_usage();
            ExitCode::from(2)
        }
    }
}
