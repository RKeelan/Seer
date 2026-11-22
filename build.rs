//! Embeds the application manifest and icon in both executables.
//!
//! The icon is drawn here rather than checked in: a teal scrying orb with a
//! cream rim on a gold stand, on a transparent background, simple enough to
//! read at 16 pixels.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Icon sizes, in pixels, written into the `.ico`.
const ICON_SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

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

/// The icon's colour at one pixel, as straight BGRA, averaged over an 8×8
/// grid of samples for smooth edges, including the stand's diagonal legs.
fn pixel(size: u32, column: u32, row: u32) -> [u8; 4] {
    const SAMPLES: u32 = 8;
    let mut sum = [0.0f32; 4];
    for sy in 0..SAMPLES {
        for sx in 0..SAMPLES {
            // Map the sample onto the 256-unit design grid.
            let scale = GRID / size as f32;
            let x = (column as f32 + (sx as f32 + 0.5) / SAMPLES as f32) * scale;
            let y = (row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) * scale;
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

/// The width and height of the grid the icon is designed on.
const GRID: f32 = 256.0;

/// The icon at a point on the design grid, y downwards, as straight RGBA in
/// [0, 1]: a scrying orb on a stand, with nothing behind it. The orb is a
/// dark teal disc with a cream rim and a cream highlight arcing over its
/// upper left. It sits on a gold stand: a rounded collar bar with two legs
/// splayed out beneath it.
fn sample(x: f32, y: f32) -> [f32; 4] {
    const TEAL: [f32; 4] = opaque(0x0F3D3E);
    const CREAM: [f32; 4] = opaque(0xF0E9CF);
    const GOLD: [f32; 4] = opaque(0xD4A843);

    // The orb is drawn over the stand.
    const CENTRE: (f32, f32) = (128.0, 102.0);
    const RIM_OUTER: f32 = 100.0;
    const RIM_INNER: f32 = 88.0;
    let (dx, dy) = (x - CENTRE.0, y - CENTRE.1);
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= RIM_OUTER {
        return if distance < RIM_INNER {
            if in_highlight(dx, dy, distance) {
                CREAM
            } else {
                TEAL
            }
        } else {
            CREAM
        };
    }

    // The collar is a bar with fully rounded ends, centred on y = 202.
    const COLLAR_LEFT: f32 = 66.0;
    const COLLAR_RIGHT: f32 = 190.0;
    const COLLAR_Y: f32 = 202.0;
    const COLLAR_RADIUS: f32 = 14.0;
    let cx = x.clamp(COLLAR_LEFT, COLLAR_RIGHT);
    let in_collar = (x - cx).powi(2) + (y - COLLAR_Y).powi(2) <= COLLAR_RADIUS * COLLAR_RADIUS;

    // Each leg is a quadrilateral running from under the collar down to the
    // bottom edge; the right leg mirrors the left about x = 128.
    const LEG: [(f32, f32); 4] = [(76.0, 204.0), (44.0, 252.0), (80.0, 252.0), (104.0, 210.0)];
    let in_leg = in_polygon(&LEG, x, y) || in_polygon(&LEG, GRID - x, y);

    if in_collar || in_leg { GOLD } else { [0.0; 4] }
}

/// An opaque colour given as `0xRRGGBB`, as straight RGBA in [0, 1].
const fn opaque(rgb: u32) -> [f32; 4] {
    [
        ((rgb >> 16) & 0xFF) as f32 / 255.0,
        ((rgb >> 8) & 0xFF) as f32 / 255.0,
        (rgb & 0xFF) as f32 / 255.0,
        1.0,
    ]
}

/// Whether a point, given relative to the orb's centre, lies on the
/// highlight: a quarter circle of radius 66 from due left to due up, stroked
/// 18 wide with round caps.
fn in_highlight(dx: f32, dy: f32, distance: f32) -> bool {
    const RADIUS: f32 = 66.0;
    const HALF_WIDTH: f32 = 9.0;
    if dx <= 0.0 && dy <= 0.0 {
        return (distance - RADIUS).abs() <= HALF_WIDTH;
    }
    let near = |ex: f32, ey: f32| (dx - ex).powi(2) + (dy - ey).powi(2) <= HALF_WIDTH * HALF_WIDTH;
    near(-RADIUS, 0.0) || near(0.0, -RADIUS)
}

/// Whether a point lies inside a polygon, by counting the edges a ray from it
/// crosses.
fn in_polygon(vertices: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut inside = false;
    let mut previous = vertices[vertices.len() - 1];
    for &(vx, vy) in vertices {
        let (px, py) = previous;
        if (vy > y) != (py > y) && x < vx + (y - vy) * (px - vx) / (py - vy) {
            inside = !inside;
        }
        previous = (vx, vy);
    }
    inside
}
