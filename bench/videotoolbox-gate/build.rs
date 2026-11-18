//! libvpx's encoder API and the VP9 controls, as voxl's build.rs binds them.
fn main() {
    let vpx = pkg_config::probe_library("vpx").expect("vpx");
    let mut v = bindgen::Builder::default()
        .header_contents(
            "vpx.h",
            "#include <vpx/vpx_encoder.h>\n#include <vpx/vp8cx.h>\n",
        )
        .allowlist_function("vpx_.*")
        .allowlist_type("vpx_.*|vp8e_.*|vp9e_.*|VP8E_.*|VP9E_.*")
        .allowlist_var("VPX_.*|VP8_.*|VP9_.*|VP8E_.*|VP9E_.*")
        .layout_tests(false);
    for p in &vpx.include_paths {
        v = v.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let code = v.generate().expect("bindgen libvpx").to_string();
    std::fs::write(out.join("vpx.rs"), code).unwrap();
    for p in &vpx.link_paths {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
