//! Vendor compatibility configuration; `cortex-config` owns it.

pub(crate) use cortex_config::compat::INSTRUCTION_FILENAMES;
pub use cortex_config::compat::{
    COMPAT_CELLS, CompatCell, CompatConfig, CompatConfigToml, CompatRemoteKey, CompatSurface,
    CompatVendor, VendorCompat, VendorCompatToml,
};
