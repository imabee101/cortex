fn main() {
    println!("cargo:rerun-if-env-changed=CORTEX_VERSION");
}
