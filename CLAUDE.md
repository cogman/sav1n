# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with the `sav1n` repository.

## Common Development Commands

- **Build the project**: `cargo build --release`
- **Run the application**: `cargo run --release -- [options]`
  - Example: `cargo run --release -- -i "input.mkv" -v "script.vpy" -e 4 -t 95 -c 2 -o av1`
- **Run tests** (if any are added): `cargo test`
- **Run a single test**: `cargo test <test_name>`
- **Check formatting**: `cargo fmt -- --check`
- **Lint / static analysis**: `cargo clippy -- -D warnings`
- **Update dependencies**: `cargo update`

## High‑Level Architecture Overview

- **Entry point (`src/main.rs`)**: Asynchronous `#[tokio::main]` function that parses CLI arguments using `clap`, resolves input file glob patterns, and orchestrates the encoding pipeline.
- **Concurrency model**: Uses Tokio's async runtime with semaphores to limit parallel encoders (`encoders` CLI flag) and to coordinate scene processing.
- **Core pipeline steps**:
  1. **Audio extraction** – `ffprobe` to discover streams, then `ffmpeg` to encode audio to Opus.
  2. **Video preprocessing** – `vspipe` runs a VapourSynth script, producing Y4M frames and optional timecodes.
  3. **Scene detection** – First‑pass AOM (AV1) encoding writes keyframe statistics; these are consumed by a stats processor to decide scene boundaries.
  4. **Scene encoding** – Each scene is encoded twice (first‑pass for quality estimation, second‑pass for final output) using either the VP9 (`Vp9Encoder`) or AV1 (`Av1Encoder`) implementation behind the `Encoder` trait.
  5. **VMAF quality search** – A secant search adjusts the constant quality (CQ) value per scene to meet the target VMAF score.
  6. **Muxing** – `mkvmerge` concatenates the per‑scene IVF files, audio track, and optional timestamps into the final MKV.
- **Key modules**:
  - `src/encoder.rs` – Defines the `Encoder` trait and concrete implementations for VP9 and AV1.
  - `src/frame.rs`, `src/frame_buffer.rs` – Frame handling and buffering logic.
  - `src/video_header.rs` – Reads/writes Y4M headers.
  - `src/aom_firstpass.rs` – Parses AOM first‑pass logs for scene detection.
  - `src/vp9_encoder.rs` / `src/av1_encoder.rs` – Wrapper around external encoders (`vpxenc`, `aomenc`).
- **External dependencies**: The tool orchestrates several command‑line utilities (`ffprobe`, `ffmpeg`, `mkvmerge`, `aomenc`, `vpxenc`, `vspipe`). These must be installed and available in `$PATH`.
- **Configuration**: CLI flags control input files, VapourSynth script, number of parallel encoders, target VMAF, CPU usage for encoding, and codec selection.

## Notes for Claude Code
- When running commands, prefer the standard Rust tooling (`cargo ...`).
- For any modifications, ensure `Cargo.lock` stays in sync with `Cargo.toml`.
- The repository relies heavily on external binaries; make sure they are present before executing the pipeline.
- The codebase is primarily asynchronous Rust; avoid introducing blocking calls unless wrapped in a dedicated thread pool.
