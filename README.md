<div align="center">
  <h1><code>wasmtime</code></h1>

  <p>
    <strong>A standalone runtime for
    <a href="https://webassembly.org/">WebAssembly</a></strong>
  </p>

  <strong>A <a href="https://bytecodealliance.org/">Bytecode Alliance</a> project</strong>

  <p>
    <a href="https://github.com/bytecodealliance/wasmtime/actions?query=workflow%3ACI"><img src="https://github.com/bytecodealliance/wasmtime/workflows/CI/badge.svg" alt="build status" /></a>
    <a href="https://bytecodealliance.zulipchat.com/#narrow/stream/217126-wasmtime"><img src="https://img.shields.io/badge/zulip-join_chat-brightgreen.svg" alt="zulip chat" /></a>
    <img src="https://img.shields.io/badge/rustc-stable+-green.svg" alt="supported rustc stable" />
    <a href="https://docs.rs/wasmtime"><img src="https://docs.rs/wasmtime/badge.svg" alt="Documentation Status" /></a>
  </p>

  <h3>
    <a href="https://bytecodealliance.github.io/wasmtime/">Guide</a>
    <span> | </span>
    <a href="https://bytecodealliance.github.io/wasmtime/contributing.html">Contributing</a>
    <span> | </span>
    <a href="https://wasmtime.dev/">Website</a>
    <span> | </span>
    <a href="https://bytecodealliance.zulipchat.com/#narrow/stream/217126-wasmtime">Chat</a>
  </h3>
</div>

## Fork differences

This fork extends [upstream Wasmtime][Wasmtime] with host-requested shrinking of
WebAssembly linear memory. It is intended for guest allocators such as CIT Alloc
that can tell their C host when the unused tail of a memory can be released.

### Shrink linear memory from C

The new `wasmtime_memory_shrink` C API reduces a memory to an absolute target size
in WebAssembly pages. It releases the backing pages in the removed tail and
updates the runtime's bookkeeping, so both C size queries and Wasm's
`memory.size` instruction observe the smaller memory. The base pointer and
virtual address reservation remain unchanged. Whole host pages are decommitted;
any partial host page is retained. Removed contents are discarded, and growing
the memory again exposes zeroed bytes. The memory's declared minimum and maximum
do not change.

Build and link the [C API from this checkout](crates/c-api/README.md) to use the
extension; the upstream binaries linked below do not include this fork's
addition. Both MSVC and MSYS2/UCRT64 builds are supported.

The guest-to-host command is application-defined: the embedder supplies the
callback or import used by CIT Alloc. That callback must queue the requested
target size rather than shrink immediately. After the outermost Wasm call has
returned to C, apply the queued request:

```c
uint64_t previous_pages;
wasmtime_error_t *error = wasmtime_memory_shrink(
    context, &memory, target_pages, &previous_pages);
```

Here `context` belongs to the store owning the `wasmtime_memory_t` handle
`memory`, and `target_pages` is the new total page count, not the number of pages
to remove. Obtain the page size with `wasmtime_memory_page_size`; the default is
64 KiB. Success returns `NULL` and writes the old page count to `previous_pages`.
Failure leaves the memory and output argument unchanged; the caller owns the
error and must release it with `wasmtime_error_delete` after handling it.

The guest allocator must have relinquished the entire removed tail, and host
pointers or slices into that tail must no longer be used. If the guest runs
again before a queued request is applied, revalidate or cancel the request so
that it cannot discard allocations that have reused the tail.

The current implementation has these restrictions:

* Shrinking requires all calls into the store to have returned, including
  suspended asynchronous calls.
* The target cannot exceed the current size or fall below the minimum required
  by the defining memory or any instantiated module importing it.
* Only non-shared core memories are supported. The default mmap allocator and
  pooling without memory protection keys support shrinking. The malloc-backed
  allocator and custom allocators without shrink support reject size reductions.
* Stores with component instances, allocated Wasm continuations, or `wmemcheck`
  enabled are unsupported.

This is a host extension, not a WebAssembly `memory.shrink` instruction. Rust
embedders can use `Memory::shrink` with the same behavior and restrictions. See
the [C API reference in the header](crates/c-api/include/wasmtime/memory.h) and
the [complete C example](examples/memory-shrink.c) for details. The example
demonstrates a deferred callback, error handling, and zeroed regrowth; it is
available as the `wasmtime-memory-shrink` CMake target in `examples`.

### Rebuild with MSVC

On Windows, run [build-msvc.ps1](build-msvc.ps1) from PowerShell to rebuild and
install this fork's x64 MSVC C API:

```powershell
.\build-msvc.ps1
```

The script finds Visual Studio and initializes its developer environment, selects
the MSVC Rust toolchain, reconfigures CMake, and builds in Release mode. Headers,
the static library, the DLL, and its import library are installed under
`artifacts/msvc`. Existing build artifacts are reused. Visual Studio C++ build
tools with a Windows SDK, rustup, CMake, and Ninja must already be installed.

Use `-UpdateRust` after an update that requires a newer Rust compiler, `-BuildCli`
to also build `target/msvc-cli/x86_64-pc-windows-msvc/release/wasmtime.exe`, or
`-Offline` when the toolchain and dependencies are already cached. The CLI uses a
separate Cargo output directory so its import library cannot overwrite the C API
static library. The build runs in a child PowerShell process, leaving the caller's
working directory and environment unchanged even if a build fails.

If PowerShell blocks scripts, use an execution-policy override limited to a new
PowerShell process:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\build-msvc.ps1
```

## Installation

The Wasmtime CLI can be installed on Linux and macOS (locally) with a small install
script:

```console
curl https://wasmtime.dev/install.sh -sSf | bash
```
This script installs into `$WASMTIME_HOME` (defaults to `$HOME/.wasmtime`), and executable is placed in `$WASMTIME_HOME/bin`.

After running the install script above, follow the on-screen instructions.

Windows or otherwise interested users can download installers and
binaries directly from the [GitHub
Releases](https://github.com/bytecodealliance/wasmtime/releases) page.

For additional installation options, refer to the [online book CLI installation page](https://docs.wasmtime.dev/cli-install.html).

Documentation on Wasmtime's currently supported versions can be found [in the
online book
documentation](https://docs.wasmtime.dev/stability-release.html#current-versions).

## Example

If you've got the [Rust compiler
installed](https://www.rust-lang.org/tools/install) then you can take some Rust
source code:

```rust
fn main() {
    println!("Hello, world!");
}
```

and compile it into a WebAssembly component with:

```console
rustup target add wasm32-wasip2
rustc hello.rs --target wasm32-wasip2
```

Once compiled, you can run your component:

```console
wasmtime hello.wasm
```

You should see the following output:

```text
Hello, world!
```

(Note: make sure you installed Rust using the [`rustup`][rustup] method in the official
instructions above, and do not have a copy of the Rust toolchain installed on
your system in some other way as well (e.g. the system package manager). Otherwise, the `rustup target add...`
command may not install the target for the correct copy of Rust.)

[rustup]: https://rustup.rs

## Features

* **Fast**. Wasmtime is built on the optimizing [Cranelift] code generator to
  quickly generate high-quality machine code either at runtime or
  ahead-of-time. Wasmtime is optimized for efficient instantiation, low-overhead
  calls between the embedder and wasm, and scalability of concurrent instances.

* **[Secure]**. Wasmtime's development is strongly focused on correctness and
  security. Building on top of Rust's runtime safety guarantees, each Wasmtime
  feature goes through careful review and consideration via an [RFC
  process]. Once features are designed and implemented, they undergo 24/7
  fuzzing donated by [Google's OSS Fuzz]. As features stabilize they become part
  of a [release][release policy], and when things go wrong we have a
  well-defined [security policy] in place to quickly mitigate and patch any
  issues. We follow best practices for defense-in-depth and integrate
  protections and mitigations for issues like Spectre. Finally, we're working to
  push the state-of-the-art by collaborating with academic researchers to
  formally verify critical parts of Wasmtime and Cranelift.

* **[Configurable]**. Wasmtime uses sensible defaults, but can also be
  configured to provide more fine-grained control over things like CPU and
  memory consumption. Whether you want to run Wasmtime in a tiny environment or
  on massive servers with many concurrent instances, we've got you covered.

* **[WASI]**. Wasmtime supports a rich set of APIs for interacting with the host
  environment through the [WASI standard](https://wasi.dev).

* **[Standards Compliant]**. Wasmtime passes the [official WebAssembly test
  suite](https://github.com/WebAssembly/testsuite), implements the [official C
  API of wasm](https://github.com/WebAssembly/wasm-c-api), and implements
  [future proposals to WebAssembly](https://github.com/WebAssembly/proposals) as
  well. Wasmtime developers are intimately engaged with the WebAssembly
  standards process all along the way too.

[Wasmtime]: https://github.com/bytecodealliance/wasmtime
[Cranelift]: https://cranelift.dev/
[Google's OSS Fuzz]: https://google.github.io/oss-fuzz/
[security policy]: https://bytecodealliance.org/security
[RFC process]: https://github.com/bytecodealliance/rfcs
[release policy]: https://docs.wasmtime.dev/stability-release.html
[Secure]: https://docs.wasmtime.dev/security.html
[Configurable]: https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html
[WASI]: https://docs.rs/wasmtime-wasi/latest/wasmtime_wasi/
[Standards Compliant]: https://docs.wasmtime.dev/stability-tiers.html

## Language Support

You can use Wasmtime from a variety of different languages through embeddings of
the implementation.

Languages supported by the Bytecode Alliance:

* **[Rust]** - the [`wasmtime` crate]
* **[C]** - the [`wasm.h`, `wasi.h`, and `wasmtime.h` headers][c-headers], [CMake](crates/c-api/CMakeLists.txt)
* **C++** - the [`wasmtime.hh` header][c-headers]
* **[Python]** - the [`wasmtime` PyPI package]
* **[.NET]** - the [`Wasmtime` NuGet package]
* **[Go]** - the [`wasmtime-go` repository]
* **[Ruby]** - the [`wasmtime` gem]

Languages supported by the community:

* **[Elixir]** - the [`wasmex` hex package]
* **Perl** - the [`Wasm` Perl package's `Wasm::Wasmtime`]

[Rust]: https://bytecodealliance.github.io/wasmtime/lang-rust.html
[C]: https://bytecodealliance.github.io/wasmtime/lang-c.html
[`wasmtime` crate]: https://crates.io/crates/wasmtime
[c-headers]: https://bytecodealliance.github.io/wasmtime/c-api/
[Python]: https://bytecodealliance.github.io/wasmtime/lang-python.html
[`wasmtime` PyPI package]: https://pypi.org/project/wasmtime/
[.NET]: https://bytecodealliance.github.io/wasmtime/lang-dotnet.html
[`Wasmtime` NuGet package]: https://www.nuget.org/packages/Wasmtime
[Go]: https://bytecodealliance.github.io/wasmtime/lang-go.html
[`wasmtime-go` repository]: https://pkg.go.dev/github.com/bytecodealliance/wasmtime-go
[Ruby]: https://bytecodealliance.github.io/wasmtime/lang-ruby.html
[`wasmtime` gem]: https://rubygems.org/gems/wasmtime
[Elixir]: https://docs.wasmtime.dev/lang-elixir.html
[`wasmex` hex package]: https://hex.pm/packages/wasmex
[`Wasm` Perl package's `Wasm::Wasmtime`]: https://metacpan.org/pod/Wasm::Wasmtime

## Documentation

[📚 Read the Wasmtime guide here! 📚][guide]

The [wasmtime guide][guide] is the best starting point to learn about what
Wasmtime can do for you or help answer your questions about Wasmtime. If you're
curious in contributing to Wasmtime, [it can also help you do
that][contributing]!

[contributing]: https://bytecodealliance.github.io/wasmtime/contributing.html
[guide]: https://bytecodealliance.github.io/wasmtime

---

It's Wasmtime.
