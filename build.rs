//! Embeds the application manifest and icon in both executables.
//!
//! The icon is a placeholder drawn here rather than a checked-in file: a light
//! lens with a dark pupil on a rounded slate square.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Icon sizes, in pixels, written into the `.ico`.
const ICON_SIZES: [u32; 5] = [16, 24, 32, 48, 64];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by Cargo"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("set by Cargo"));
    let manifest = manifest_dir.join("res").join("seer.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", manifest.display());

    let icon = out_dir.join("seer.ico");
    fs::write(&icon, ico(&ICON_SIZES)).expect("write the icon");

    let rc = out_dir.join("seer.rc");
    let script = format!(
        "1 ICON \"{}\"\n1 24 \"{}\"\n",
        rc_path(&icon),
        rc_path(&manifest)
    );
    fs::write(&rc, script).expect("write the resource script");

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("compile the resource script");
}

/// A path as a resource-script string literal body, with backslashes escaped.
fn rc_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "\\\\")
}

/// An `.ico` file holding one 32-bit image per size.
fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&size| dib(size)).collect();
    let count = u16::try_from(sizes.len()).expect("few sizes");
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes()); // reserved
    out.extend(1u16.to_le_bytes()); // type: icon
    out.extend(count.to_le_bytes());
    let mut offset = 6 + 16 * u32::from(count);
    for (&size, image) in sizes.iter().zip(&images) {
        let dimension = if size >= 256 { 0 } else { size as u8 };
        out.push(dimension); // width
        out.push(dimension); // height
        out.push(0); // colour count
        out.push(0); // reserved
        out.extend(1u16.to_le_bytes()); // planes
        out.extend(32u16.to_le_bytes()); // bits per pixel
        let length = u32::try_from(image.len()).expect("small image");
        out.extend(length.to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += length;
    }
    for image in images {
        out.extend(image);
    }
    out
}

/// One icon image as a device-independent bitmap: a `BITMAPINFOHEADER`, the
/// BGRA rows bottom-up, then an all-clear AND mask (alpha does the masking).
fn dib(size: u32) -> Vec<u8> {
    let mask_stride = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend(40u32.to_le_bytes()); // header size
    out.extend((size as i32).to_le_bytes()); // width
    out.extend((2 * size as i32).to_le_bytes()); // height of colour and mask together
    out.extend(1u16.to_le_bytes()); // planes
    out.extend(32u16.to_le_bytes()); // bits per pixel
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend((size * size * 4 + size * mask_stride).to_le_bytes()); // image size
    out.extend([0u8; 16]); // resolution and palette fields
    for row in (0..size).rev() {
        for column in 0..size {
            out.extend(pixel(size, column, row));
        }
    }
    out.extend(vec![0u8; (size * mask_stride) as usize]);
    out
}

/// The icon's colour at one pixel, as straight BGRA, averaged
/// over a 4×4 grid of samples for smooth edges.
fn pixel(size: u32, column: u32, row: u32) -> [u8; 4] {
    const SAMPLES: u32 = 4;
    let mut sum = [0.0f32; 4];
    for sy in 0..SAMPLES {
        for sx in 0..SAMPLES {
            // Map the sample to [-1, 1] across the icon.
            let x =
                ((column as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / size as f32) * 2.0 - 1.0;
            let y = ((row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / size as f32) * 2.0 - 1.0;
            let [r, g, b, a] = sample(x, y);
            sum[0] += r * a;
            sum[1] += g * a;
            sum[2] += b * a;
            sum[3] += a;
        }
    }
    let n = (SAMPLES * SAMPLES) as f32;
    let alpha = sum[3] / n;
    let channel = |premultiplied: f32| {
        if alpha == 0.0 {
            0
        } else {
            (premultiplied / n / alpha * 255.0).round() as u8
        }
    };
    [
        channel(sum[2]),
        channel(sum[1]),
        channel(sum[0]),
        (alpha * 255.0).round() as u8,
    ]
}

/// The icon at a point in [-1, 1]², as straight RGBA in [0, 1].
fn sample(x: f32, y: f32) -> [f32; 4] {
    // A rounded square with corner radius 0.35 inside the unit box.
    let corner = 0.35;
    let qx = (x.abs() - (1.0 - corner)).max(0.0);
    let qy = (y.abs() - (1.0 - corner)).max(0.0);
    if (qx * qx + qy * qy).sqrt() > corner {
        return [0.0; 4];
    }
    let radius = (x * x + y * y).sqrt();
    if radius < 0.25 {
        [0.11, 0.13, 0.17, 1.0] // pupil
    } else if radius < 0.6 {
        [0.36, 0.72, 0.94, 1.0] // iris
    } else {
        [0.17, 0.20, 0.26, 1.0] // background
    }
}
