// Resolution (bundled binary, RG_BIN_PATH, Bazel runfiles, PATH) lives in the cortex-tools crate
// This module only preserves the `crate::util::ripgrep` path
pub use cortex_tools::util::rg_path;
