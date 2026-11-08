fn main() {
    // cargo otherwise reuses the compiled Windows .res when only artwork changes.
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
