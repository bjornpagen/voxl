# Development shell: the native libraries, plus esbuild and TypeScript for the web
# client, and FFmpeg, which the encoder's tests decode with. Rust comes from rustup, as
# rust-toolchain.toml names it.
{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [pkg-config clang rustPlatform.bindgenHook esbuild typescript ffmpeg];
  buildInputs = with pkgs; [libvpx libyuv libopus libpulseaudio xorg.libX11 xorg.libXext xorg.libxcb];
}
