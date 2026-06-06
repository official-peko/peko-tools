#!/bin/sh

if [ -v BUILDBINS ]; then
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    CROSS_CONTAINER_OPTS="--platform linux/amd64" cross build --release --target aarch64-unknown-linux-gnu
    CROSS_CONTAINER_OPTS="--platform linux/amd64" cross build --release --target x86_64-unknown-linux-gnu
    cargo xwin build --release --target x86_64-pc-windows-msvc
fi

rm -rf binaries
mkdir binaries
tar -czf binaries/peko-ls-aarch64-apple-darwin.tar.gz -C target/aarch64-apple-darwin/release peko_lsp
tar -czf binaries/peko-ls-x86_64-apple-darwin.tar.gz -C target/x86_64-apple-darwin/release peko_lsp
tar -czf binaries/peko-ls-x86_64-unknown-linux-gnu.tar.gz -C target/x86_64-unknown-linux-gnu/release peko_lsp
tar -czf binaries/peko-ls-aarch64-unknown-linux-gnu.tar.gz -C target/aarch64-unknown-linux-gnu/release peko_lsp
zip binaries/peko-ls-x86_64-pc-windows-msvc.zip -j target/x86_64-pc-windows-msvc/release/peko_lsp.exe
