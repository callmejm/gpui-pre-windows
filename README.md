# gpui-pre-windows (rustyfiles fork)

A fork of the Windows platform layer of [GPUI](https://gpui.rs), the UI framework behind the
[Zed](https://github.com/zed-industries/zed) editor. It is based on the crates.io package
`gpui-pre-windows` 0.3.11 (Zed commit `f816e1f`, crate name `gpui_windows`) and is used by
[rustyfiles](https://github.com/gelenmedya/rustyfiles).

What it adds: the window keeps drawing when the GPU or the CPU is saturated. It hot-swaps the
whole window between a hardware Direct3D 11 device and WARP, Microsoft's software rasterizer,
without a visible flicker. The design, the switching policy and every changed file are
described in [FORK.md](FORK.md).

## Use

```toml
[patch.crates-io]
gpui-pre-windows = { git = "https://github.com/gelenmedya/gpui-pre-windows", rev = "<commit>" }
```

The switch API lives in `gpui_windows::hybrid`; see FORK.md.

## License

Apache-2.0, like GPUI itself. See [LICENSE-APACHE](LICENSE-APACHE). The changes in this fork
are released under the same license.
