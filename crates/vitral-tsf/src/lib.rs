use vitral_tokens::TOKENS;

pub fn tsf_brand_name() -> &'static str {
    TOKENS.brand.name
}

pub fn tsf_abi_version() -> u32 {
    vitral_ffi::vitral_abi_version()
}

#[cfg(windows)]
mod service;
