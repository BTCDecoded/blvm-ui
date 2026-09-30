//! Settings that can be baked in when the binary is built and overridden at runtime.
//!
//! Build time: `BLVM_UI_SELF_GEO=timezone cargo build --release`
//! Runtime:    `BLVM_UI_SELF_GEO=off ./blvm-ui-next`
//!
//! Runtime wins. Cargo rebuilds automatically when a baked-in value changes.

/// Runtime env var, else the value set when the binary was built (`option_env!`).
pub fn setting(name: &str, built: Option<&'static str>) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| built.filter(|v| !v.trim().is_empty()).map(str::to_string))
}
