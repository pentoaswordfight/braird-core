// In-crate uniffi-bindgen (library mode). Generates the Swift/Kotlin/Python
// bindings from the compiled cdylib:
//   cargo run --features uniffi/cli --bin uniffi-bindgen -- generate \
//     --library target/release/libbraird_core.dylib --language swift --out-dir <dir>
fn main() {
    uniffi::uniffi_bindgen_main()
}
