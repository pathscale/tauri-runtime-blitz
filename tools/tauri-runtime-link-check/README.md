# Tauri runtime link check

The published macOS `tauri-runtime` crate exposes WebKit types even when a custom runtime never
uses them. This tiny release binary proves that the repository's `-dead_strip_dylibs` setting
removes the unused framework load command.

Run `./verify.sh`. It fails if the final Mach-O links WebKit, `libc++`, or Python.
