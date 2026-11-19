fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS");
    let include_paths = unix_libs(&target);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    // libaom's AV1 encoder: the items src/av1.rs uses and no others, in Rust's case, so the
    // module that includes them needs no lint of its own allowed.
    let mut b = bindgen::Builder::default()
        .header_contents(
            "aom.h",
            "#include <aom/aom_encoder.h>\n#include <aom/aomcx.h>\n",
        )
        .parse_callbacks(Box::new(RustCase))
        .prepend_enum_name(false)
        .derive_debug(false)
        .layout_tests(false);
    for item in AOM_ITEMS {
        b = b.allowlist_item(item);
    }
    for p in &include_paths {
        b = b.clang_arg(format!("-I{}", p.display()));
    }
    let aom = b.generate().expect("bindgen libaom").to_string();
    std::fs::write(out.join("aom.rs"), aom).unwrap();
    bundle_web(&out);
    println!("cargo:rerun-if-changed=build.rs");
}

/// The libaom items src/av1.rs uses, each by its whole name: functions, the macros it reads,
/// and the enums whose values it names (their other values are hidden, `AOM_VALUES`).
const AOM_ITEMS: &[&str] = &[
    "aom_codec_av1_cx",
    "aom_codec_enc_config_default",
    "aom_codec_enc_init_ver",
    "aom_codec_control",
    "aom_codec_encode",
    "aom_codec_get_cx_data",
    "aom_codec_get_global_headers",
    "aom_codec_destroy",
    "aom_img_wrap",
    "AOM_ENCODER_ABI_VERSION",
    "AOM_USAGE_REALTIME",
    "AOM_EFLAG_FORCE_KF",
    "AOM_FRAME_IS_KEY",
    "aome_enc_control_id",
    "aom_tune_content",
    "aom_color_primaries",
    "aom_transfer_characteristics",
    "aom_matrix_coefficients",
    "aom_color_range",
    "aom_superblock_size",
];

/// The enum values src/av1.rs names: every other value of libaom's enums is left out of the
/// bindings, so none is bound unused.
const AOM_VALUES: &[&str] = &[
    "AOM_CODEC_OK",
    "AOM_CODEC_ERROR",
    "AOM_CODEC_CX_FRAME_PKT",
    "AOM_KF_DISABLED",
    "AOM_IMG_FMT_I420",
    "AOM_CONTENT_SCREEN",
    "AOM_CICP_CP_BT_709",
    "AOM_CICP_TC_SRGB",
    "AOM_CICP_MC_BT_601",
    "AOM_CR_STUDIO_RANGE",
    "AOME_SET_CPUUSED",
    "AV1E_SET_TUNE_CONTENT",
    "AV1E_SET_ENABLE_PALETTE",
    "AV1E_SET_ENABLE_INTRABC",
    "AV1E_SET_COEFF_COST_UPD_FREQ",
    "AV1E_SET_MODE_COST_UPD_FREQ",
    "AV1E_SET_MV_COST_UPD_FREQ",
    "AV1E_SET_DV_COST_UPD_FREQ",
    "AV1E_SET_COLOR_PRIMARIES",
    "AV1E_SET_TRANSFER_CHARACTERISTICS",
    "AV1E_SET_MATRIX_COEFFICIENTS",
    "AV1E_SET_COLOR_RANGE",
    "AV1E_SET_ROW_MT",
    "AV1E_SET_TILE_COLUMNS",
    "AV1E_SET_SUPERBLOCK_SIZE",
    "AOM_SUPERBLOCK_SIZE_64X64",
];

/// Names libaom's C items as Rust names its own, and binds only the enum values src/av1.rs
/// names: a type in upper camel case, without C's `_t` (`aom_codec_ctx_t` is
/// `AomCodecCtx`); functions, fields and constants are already in Rust's case.
#[derive(Debug)]
struct RustCase;

impl bindgen::callbacks::ParseCallbacks for RustCase {
    fn item_name(&self, item: bindgen::callbacks::ItemInfo) -> Option<String> {
        let words = item.name.trim_end_matches("_t").split('_');
        let camel = words
            .filter(|w| !w.is_empty())
            .map(|w| w[..1].to_ascii_uppercase() + &w[1..])
            .collect();
        matches!(item.kind, bindgen::callbacks::ItemKind::Type).then_some(camel)
    }

    fn enum_variant_behavior(
        &self,
        _enum_name: Option<&str>,
        variant: &str,
        _value: bindgen::callbacks::EnumVariantValue,
    ) -> Option<bindgen::callbacks::EnumVariantCustomBehavior> {
        (!AOM_VALUES.contains(&variant))
            .then_some(bindgen::callbacks::EnumVariantCustomBehavior::Hide)
    }
}

/// libaom, libyuv, Opus and PulseAudio from pkg-config; returns libaom's include paths. The
/// daemon runs on Linux alone; elsewhere its core is built for its tests, which code
/// with libaom and nothing else.
fn unix_libs(target: &str) -> Vec<std::path::PathBuf> {
    let aom = pkg_config::probe_library("aom").expect("aom");
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
    let dirs: Vec<_> = std::iter::once(&aom)
        .chain(others.iter())
        .flat_map(|lib| &lib.link_paths)
        .collect();
    for p in &dirs {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", p.display());
    }
    let rpath = std::env::join_paths(&dirs).expect("library dirs");
    println!("cargo:rpath={}", rpath.to_string_lossy());
    aom.include_paths
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
