# Forge

Forge is being rewritten as a Rust + Tauri native workshop for agentic work. Any AI. One editor. Transparent by default.

See [`docs/product/vision.md`](docs/product/vision.md) for the product vision and [`docs/build/roadmap.md`](docs/build/roadmap.md) for the build plan.

**Dev quickstart.** Install [`just`](https://github.com/casey/just) (`cargo install just`, `brew install just`, or `apt install just`), then run `just` from the repo root to see all dev recipes. `just dev` launches the full Tauri + Vite loop; `just check` runs the same lint gates CI does. See [`AGENTS.md`](AGENTS.md#build-commands) for the full recipe list.

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE-2.0) or [MIT License](LICENSE-MIT) at your option.

Supply-chain scanners and suppression policy: [`docs/dev/security.md`](docs/dev/security.md).

Legacy VS Code fork preserved at tag [`legacy-vscode-fork`](https://github.com/forge-ide/forge/tree/legacy-vscode-fork).
