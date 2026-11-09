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
  4. **Scene encoding** – Each scene is written to disk as a lossless FFV1 Matroska file, decoded back to Y4M on a pipe, and encoded using the VP9 (`Vp9Encoder`), AV1 (`Av1Encoder`), or x265 (`X265Encoder`) implementation behind the `Encoder` trait. VP9 and AV1 use a two‑pass flow (first‑pass for quality estimation, second‑pass for final output); x265 uses single‑pass CRF.
  5. **Perceptual quality search** – A metric-independent Brent-style integer search adjusts CQ/CRF per scene to meet a VMAF, CVVDP, or mean per-frame Butteraugli target. Reference-only metric preparation runs once per scene (`{scene}.metric`); trial encodes and metric artifacts live in isolated probe workspaces (`{scene}.probe-{cq}`); metric scores are reported in native units.
  6. **Muxing** – `mkvmerge` concatenates the per‑scene IVF files, audio track, and optional timestamps into the final MKV.
- **Key modules**:
  - `src/encoder.rs` – Defines the `Encoder` trait and concrete implementations for VP9, AV1, and x265.
  - `src/cq_search.rs` – Metric-independent Brent-style integer search; lower-is-better scores and targets are negated at the integration boundary.
  - `src/metric.rs` – VMAF, CVVDP, and Butteraugli backends, input preparation, native score parsing, and validation.
  - `src/frame.rs`, `src/frame_buffer.rs` – Frame handling and buffering logic.
  - `src/video_header.rs` – Reads/writes Y4M headers.
  - `src/shutdown.rs` – Tracks child processes and temporary folders, and terminates them gracefully on `SIGINT`/`SIGTERM`.
  - `src/aom_firstpass.rs` – Parses AOM first‑pass logs for scene detection.
  - `src/vp9_encoder.rs` / `src/av1_encoder.rs` / `src/x265_encoder.rs` – Wrapper around external encoders (`vpxenc`, `aomenc`, `x265`).
- **External dependencies**: The tool orchestrates several command‑line utilities (`ffprobe`, `ffmpeg`, `mkvmerge`, `aomenc`, `vpxenc`, `x265`, `vspipe`). These must be installed and available in `$PATH`.
- **Configuration**: CLI flags control inputs, VapourSynth script, concurrency, metric (`--metric`), native target (`--target`), tolerance, trial/final encoding speeds, CVVDP display/device, and codec. VMAF defaults to 95; other metrics require an explicit target. Legacy VMAF flag names remain aliases.

## Notes for Claude Code
- When running commands, prefer the standard Rust tooling (`cargo ...`).
- For any modifications, ensure `Cargo.lock` stays in sync with `Cargo.toml`.
- The repository relies heavily on external binaries; make sure they are present before executing the pipeline.
- The codebase is primarily asynchronous Rust; avoid introducing blocking calls unless wrapped in a dedicated thread pool.
