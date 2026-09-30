// The decoder's memory is capped at 1 GiB (declared in the module, so a
// loader can check it before running anything): a pack that asks for more
// fails inside the decoder instead of taking the host's memory.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg=--max-memory=1073741824");
    }
}
