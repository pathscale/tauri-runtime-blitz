# CSS probe

Rust-only Stage 2 render and pixel-diff utility. It uses the same Python-free Stylo fork and
CPU renderer as the Blitz spike.

```sh
PYTHON3=/definitely/not-a-python-interpreter \
  cargo run --manifest-path tools/css-probe/Cargo.toml -- \
  render input.html actual.png 1344 900

cargo run --manifest-path tools/css-probe/Cargo.toml -- \
  diff reference.png actual.png diff.png
```

The `render` input must be settled HTML with styles inlined. The AgencyZero design corpus is a
packed JavaScript artifact, so capture its settled DOM and CSS from the browser rather than
asking Blitz to run the packer. This keeps Stage 2 focused on CSS/style/layout; running the real
application JavaScript is the Stage 3 gate.

Cargo output is shared with `../blitz-rust/target` through the repository `.cargo/config.toml`
to avoid a duplicate multi-gigabyte target directory.
