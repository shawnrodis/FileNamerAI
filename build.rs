fn main() {
    // Only embed Windows resources when targeting Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Minimal .rc: exe icon + asInvoker manifest (no admin needed for this app).
    std::fs::write(
        out.join("fb.rc"),
        "101 ICON \"assets/FileNamerAI.ico\"\n1 24 \"assets/FileNamerAI.manifest\"\n",
    )
    .unwrap();

    let status = std::process::Command::new("windres")
        .args([
            "--input-format=rc",
            "--output-format=coff",
            "--target=pe-x86-64",
        ])
        .arg(out.join("fb.rc"))
        .arg(out.join("resource.o"))
        .status()
        .expect("failed to run windres (is mingw-w64 installed?)");
    assert!(status.success(), "windres failed");

    // Link the object directly (not via an archive, which the linker can discard).
    println!("cargo:rustc-link-arg={}", out.join("resource.o").display());
    println!("cargo:rerun-if-changed=assets/FileNamerAI.ico");
    println!("cargo:rerun-if-changed=assets/FileNamerAI.manifest");
}
