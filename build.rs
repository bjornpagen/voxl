fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS");
    let include_paths = unix_libs(&target);
    let mut b = bindgen::Builder::default()
        .header_contents(
            "wrap.h",
            "#include <stdint.h>\n#include <x264.h>\n\
             void voxl_x264_log(void *, int, const char *, va_list);\n",
        )
        .allowlist_function("x264_.*")
        // x264's log callback (src/x264_log.c), declared as x264 declares pf_log.
        .allowlist_function("voxl_x264_log")
        .allowlist_type("x264_.*")
        .allowlist_var("X264_.*")
        .layout_tests(false);
    for p in &include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut code = b.generate().expect("bindgen x264").to_string();
    // x264.h renames x264_encoder_open after the library's build number (a macro
    // bindgen can't follow); alias whichever version this x264 has.
    let open = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|w| {
            w.strip_prefix("x264_encoder_open_")
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        })
        .expect("x264_encoder_open_<build> in x264.h")
        .to_string();
    code.push_str(&format!("\npub use {open} as x264_encoder_open;\n"));
    std::fs::write(out.join("x264.rs"), code).unwrap();
    cc::Build::new()
        .file("src/x264_log.c")
        .compile("voxl_x264_log");
    println!("cargo:rerun-if-changed=src/x264_log.c");
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

/// x264, libyuv, Opus and PulseAudio from pkg-config; returns x264's include paths. The
/// daemon runs on Linux alone; elsewhere its core is built for its tests, which code
/// with x264 and nothing else.
fn unix_libs(target: &str) -> Vec<std::path::PathBuf> {
    let x264 = pkg_config::probe_library("x264").expect("x264");
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
    let dirs: Vec<_> = std::iter::once(&x264)
        .chain(others.iter())
        .flat_map(|lib| &lib.link_paths)
        .collect();
    for p in &dirs {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    let rpath = std::env::join_paths(&dirs).expect("library dirs");
    println!("cargo:rpath={}", rpath.to_string_lossy());
    x264.include_paths
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
