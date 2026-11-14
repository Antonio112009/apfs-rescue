//! On macOS, link macFUSE "weakly": the program then still starts on Macs
//! without macFUSE, and only the `mount` command needs it (it checks first).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo::rustc-link-arg=-Wl,-weak-lfuse");
    }
}
