# DrDrOS

Experimental Rust desktop and userland running on the Linux kernel. Uses the framebuffer for display and Buildroot for the boot image.

This is an unfinished operating-system project. Hardware compatibility is limited. The browser supports HTTP but has no TLS implementation, and the media tools do not provide video playback.

```sh
cargo test --workspace
cargo build --release
git submodule update --init --recursive
```

See [buildroot](buildroot/README.md) and [iso](iso/README.md) for image generation. Test boot images in a virtual machine before using them on hardware.
