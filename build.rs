fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS");
    let include_paths = unix_libs(&target);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // libvpx's VP9 encoder: its encoder API and the VP9 controls.
    let mut v = bindgen::Builder::default()
        .header_contents(
            "vpx.h",
            "#include <vpx/vpx_encoder.h>\n#include <vpx/vp8cx.h>\n",
        )
        .allowlist_function("vpx_.*")
        .allowlist_type("vpx_.*|vp8e_.*|vp9e_.*|VP8E_.*|VP9E_.*")
        .allowlist_var("VPX_.*|VP8_.*|VP9_.*|VP8E_.*|VP9E_.*")
        .layout_tests(false);
    for p in &include_paths {
        v = v.clang_arg(format!("-I{}", p.display()));
    }
    let vpx = v.generate().expect("bindgen libvpx").to_string();
    std::fs::write(out.join("vpx.rs"), vpx).unwrap();
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

/// libvpx, libyuv, Opus and PulseAudio from pkg-config; returns libvpx's include paths. The
/// daemon runs on Linux alone; elsewhere its core is built for its tests, which code
/// with libvpx and nothing else.
fn unix_libs(target: &str) -> Vec<std::path::PathBuf> {
    let vpx = pkg_config::probe_library("vpx").expect("vpx");
    let libs: &[&str] = match target {
        "linux" => &["libyuv", "opus", "libpulse-simple"],
        _ => &[],
    };
    let others: Vec<_> = libs
        .iter()
        .filter_map(|&l| match pkg_config::probe_library(l) {
            // libyuv's CMake install writes no libyuv.pc, so Arch's and Debian's packages
            // have none: link it from the default library path.
            Err(_) if l == "libyuv" => {
                println!("cargo:rustc-link-lib=yuv");
                None
            }
            lib => Some(lib.expect(l)),
        })
        .collect();
    // Embed library dirs so the binary runs outside the build shell. The link args only
    // reach voxl's own binary, so programs that embed voxl get the dirs as DEP_VOXL_RPATH.
    let dirs: Vec<_> = std::iter::once(&vpx)
        .chain(others.iter())
        .flat_map(|lib| &lib.link_paths)
        .collect();
    for p in &dirs {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    let rpath = std::env::join_paths(&dirs).expect("library dirs");
    println!("cargo:rpath={}", rpath.to_string_lossy());
    vpx.include_paths
}

/// Bundles the TypeScript web client (web/src) into OUT_DIR/web, which the server
/// embeds. Needs esbuild on PATH (or ESBUILD); shell.nix provides it.
fn bundle_web(out: &std::path::Path) {
    let esbuild = std::env::var("ESBUILD").unwrap_or_else(|_| "esbuild".into());
    let status = std::process::Command::new(&esbuild)
        .args([
            "client=web/src/client/main.ts",
            "worker=web/src/worker.ts",
            "audio=web/src/worklets/audio.ts",
            "mic=web/src/worklets/mic.ts",
            "--bundle",
            "--format=iife",
            "--target=es2022",
            "--minify",
            "--log-level=warning",
        ])
        .arg(format!("--outdir={}", out.join("web").display()))
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "running {esbuild} to bundle web/src failed ({e}); use nix-shell, or set ESBUILD"
            )
        });
    assert!(status.success(), "esbuild failed to bundle web/src");
    println!("cargo:rerun-if-changed=web/src");
    println!("cargo:rerun-if-env-changed=ESBUILD");
}
