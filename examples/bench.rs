//! Measures the viewer's cold start: the time from the creation of the
//! `seerw` process to its first `Present`, for a 12-megapixel JPEG.
//!
//! ```text
//! cargo build --release
//! cargo run --release --example bench -- [--runs <n>] [--headless]
//! ```
//!
//! Each run starts `seerw` on a fresh instance with `--exit-after-first-frame`
//! and reads the timings it prints. The window is shown unless `--headless` is
//! given, because showing it is part of what is being measured. The report
//! gives each run's time, then the median and worst of the first present and
//! of the window being shown, then the median of each phase so a slow phase
//! stands out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use seer::decode::wic::Decoder;
use seer::model::app::DecodeTarget;
use seer::model::geometry::PixelSize;
use seer::protocol::Timings;
use seer::testutil::{Format, Pixels, Rgba, TempDir, unique_instance, write_image};

const WIDTH: u32 = 4000;
const HEIGHT: u32 = 3000;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("bench: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut runs = 20;
    let mut headless = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => headless = true,
            "--runs" => {
                runs = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|&n: &usize| n > 0)
                    .ok_or("--runs needs a positive number")?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }

    let seerw = seerw()?;
    let dir = TempDir::new().map_err(|error| error.to_string())?;
    let image = dir.join("photo.jpg");
    write_image(&image, &photo(), Format::Jpeg).map_err(|error| error.to_string())?;
    let bytes = std::fs::metadata(&image).map_or(0, |m| m.len());
    println!(
        "{runs} cold starts of {} on a {WIDTH} × {HEIGHT} JPEG ({:.1} MB){}",
        seerw.display(),
        bytes as f64 / 1e6,
        if headless { ", headless" } else { "" }
    );
    decode_times(&image)?;

    let mut presents = Vec::new();
    let mut phases: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for run in 1..=runs {
        let mut command = Command::new(&seerw);
        command
            .arg("--exit-after-first-frame")
            .arg("--instance")
            .arg(unique_instance());
        if headless {
            command.arg("--headless");
        }
        let output = command
            .arg(&image)
            .output()
            .map_err(|error| format!("cannot run {}: {error}", seerw.display()))?;
        if !output.status.success() {
            return Err(format!(
                "seerw exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let timings: Timings = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("seerw printed no timings: {error}"))?;
        let mut present = None;
        for phase in timings.phases {
            if phase.name == "first_present" {
                present = Some(phase.ms);
            }
            phases.entry(phase.name).or_default().push(phase.ms);
        }
        let present = present.ok_or("seerw reported no first_present")?;
        println!("run {run:>2}: {present:6.1} ms");
        presents.push(present);
    }

    println!(
        "first_present: median {:.1} ms, worst {:.1} ms",
        median(&mut presents),
        worst(&presents)
    );
    if let Some(shown) = phases.get_mut("shown") {
        println!(
            "shown: median {:.1} ms, worst {:.1} ms",
            median(shown),
            worst(shown)
        );
    }
    println!("median of each phase, in ms from process creation:");
    let mut medians: Vec<(f64, String)> = phases
        .into_iter()
        .map(|(name, mut times)| (median(&mut times), name))
        .collect();
    medians.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (ms, name) in medians {
        println!("  {ms:6.1}  {name}");
    }
    Ok(())
}

/// Prints how long the image takes to decode in this process, on a warm
/// decoder, at the preview size for a 1920 × 1080 window and in full.
fn decode_times(image: &Path) -> Result<(), String> {
    let decoder = Decoder::new().map_err(|error| error.to_string())?;
    let targets = [
        (
            "preview",
            DecodeTarget::Fit {
                within: PixelSize::new(1920, 1080),
            },
        ),
        ("full", DecodeTarget::Full),
    ];
    for (name, target) in targets {
        let mut times = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            let source = decoder.open(image).map_err(|error| error.to_string())?;
            source.decode(target).map_err(|error| error.to_string())?;
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{name} decode alone: median {:.1} ms", median(&mut times));
    }
    Ok(())
}

/// The release `seerw.exe`, beside the directory this example runs from.
fn seerw() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = exe
        .parent()
        .and_then(|examples| examples.parent())
        .map(|dir| dir.join("seerw.exe"))
        .ok_or("cannot find the build directory")?;
    if path.exists() {
        Ok(path)
    } else {
        Err(format!(
            "{} does not exist; run `cargo build --release` first",
            path.display()
        ))
    }
}

/// A 12-megapixel image with smooth gradients and fine texture, so the JPEG
/// costs about what a photograph does to decode.
fn photo() -> Pixels {
    Pixels::from_fn(WIDTH, HEIGHT, |x, y| {
        let grain = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 27;
        Rgba::opaque(
            (x * 200 / WIDTH + grain) as u8,
            (y * 200 / HEIGHT + grain) as u8,
            ((x + y) * 100 / (WIDTH + HEIGHT) + 80 + grain) as u8,
        )
    })
}

fn worst(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::MIN, f64::max)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}
