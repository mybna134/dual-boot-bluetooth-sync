fn main() {
    cc::Build::new()
        .file("vendor/ntreg/ntreg.c")
        .file("src/ntreg_bridge.c")
        .include("vendor/ntreg")
        .warnings(false)
        .compile("bls_ntreg");
    println!("cargo:rerun-if-changed=vendor/ntreg/ntreg.c");
    println!("cargo:rerun-if-changed=vendor/ntreg/ntreg.h");
    println!("cargo:rerun-if-changed=src/ntreg_bridge.c");
}
