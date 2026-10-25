//! One-off generator for `assets/icon.ico`, GameFrame's multi-resolution
//! Windows app icon (embedded into the exe by `build.rs`). Not part of the
//! shipped app -- run manually with `cargo run --example gen_icon` whenever
//! `gameframe::icon::mark_rgba`'s design changes, and commit the
//! regenerated `assets/icon.ico`.
//!
//! Frames are stored as PNG inside the `.ico` container (valid since
//! Windows Vista, and required for the 256x256 frame) rather than the
//! legacy uncompressed BMP-in-ICO format, so no bit-mask/DIB-header
//! hand-rolling is needed beyond the small ICONDIR/ICONDIRENTRY header.

use gameframe::icon::mark_rgba;
use image::{ImageBuffer, Rgba};
use std::io::Cursor;

const SIZES: [u32; 4] = [16, 32, 48, 256];

fn main() {
    let frames: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&size| (size, render_png(size))).collect();
    let ico = build_ico(&frames);

    std::fs::create_dir_all("assets").expect("failed to create assets/");
    std::fs::write("assets/icon.ico", &ico).expect("failed to write assets/icon.ico");
    println!(
        "wrote assets/icon.ico ({} frames, {} bytes)",
        frames.len(),
        ico.len()
    );
}

fn render_png(size: u32) -> Vec<u8> {
    let rgba = mark_rgba(size);
    let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(size, size, rgba).expect("mark_rgba buffer size mismatch");

    let mut png_bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .expect("PNG encoding failed");
    png_bytes
}

/// Hand-rolled ICO container: a 6-byte ICONDIR header, one 16-byte
/// ICONDIRENTRY per frame, then the raw PNG bytes back to back.
/// See the Microsoft ICO format reference for the exact field layout.
fn build_ico(frames: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();

    out.extend_from_slice(&0u16.to_le_bytes()); // reserved, must be 0
    out.extend_from_slice(&1u16.to_le_bytes()); // type: 1 = icon
    out.extend_from_slice(&(frames.len() as u16).to_le_bytes());

    let header_len = 6 + 16 * frames.len();
    let mut offset = header_len as u32;
    for (size, png) in frames {
        // Width/height fields are single bytes; 0 conventionally means 256.
        let dimension_byte = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dimension_byte);
        out.push(dimension_byte);
        out.push(0); // color palette count: 0 = no palette (true color)
        out.push(0); // reserved, must be 0
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }

    for (_, png) in frames {
        out.extend_from_slice(png);
    }
    out
}
