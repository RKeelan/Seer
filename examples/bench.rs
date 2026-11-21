//! Measures how quickly the viewer shows a 12-megapixel JPEG, in one of two
//! ways.
//!
//! ```text
//! cargo build --release
//! cargo run --release --example bench -- [--open] [--runs <n>] [--headless]
//! ```
//!
//! By default it measures the cold start: the time from the creation of the
//! `seerw` process to its first `Present`. Each run starts `seerw` on a fresh
//! instance with `--exit-after-first-frame` and reads the timings it prints.
//! The report gives each run's time, then the median and worst of the first
//! present and of the window being shown, then the median of each phase so a
//! slow phase stands out.
//!
//! With `--open` it measures an open into a running viewer: the time from
//! just before a `seer open <file>` process starts to the viewer's first
//! `Present` of that image. It starts one viewer, then opens a different copy
//! of the JPEG on each run, so every open decodes afresh. The viewer records
//! when it handled the request and presented the image; the benchmark places
//! the moment it started `seer` on the viewer's timeline with
//! [`process_age_ms`]. The report gives each run's time, the median and worst,
//! and the median of each step.
//!
//! The window is shown unless `--headless` is given, because showing it is
//! part of what is being measured.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Output, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;

use seer::decode::wic::Decoder;
use seer::ipc::client;
use seer::ipc::instance::InstanceName;
use seer::model::app::DecodeTarget;
use seer::model::geometry::PixelSize;
use seer::protocol::{Reply, Timings};
use seer::testutil::{Format, Pixels, Rgba, TempDir, VIEWER_TIMEOUT, unique_instance, write_image};
use seer::viewer::timing::process_age_ms;

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
    let mut open = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--headless" => headless = true,
            "--open" => open = true,
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

    let seerw = release_exe("seerw.exe")?;
    let dir = TempDir::new().map_err(|error| error.to_string())?;
    let image = dir.join("photo.jpg");
    write_image(&image, &photo(), Format::Jpeg).map_err(|error| error.to_string())?;
    let bytes = std::fs::metadata(&image).map_or(0, |m| m.len());
    println!(
        "{runs} {} of {} on a {WIDTH} × {HEIGHT} JPEG ({:.1} MB){}",
        if open {
            "opens into a running viewer"
        } else {
            "cold starts"
        },
        seerw.display(),
        bytes as f64 / 1e6,
        if headless { ", headless" } else { "" }
    );
    decode_times(&image)?;

    if open {
        let seer = release_exe("seer.exe")?;
        open_latency(&seerw, &seer, &dir, &image, runs, headless)
    } else {
        cold_start(&seerw, &image, runs, headless)
    }
}

fn cold_start(seerw: &Path, image: &Path, runs: usize, headless: bool) -> Result<(), String> {
    let mut presents = Vec::new();
    let mut phases: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for run in 1..=runs {
        let mut command = Command::new(seerw);
        command
            .arg("--exit-after-first-frame")
            .arg("--instance")
            .arg(unique_instance());
        if headless {
            command.arg("--headless");
        }
        let output = command
            .arg(image)
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

fn open_latency(
    seerw: &Path,
    seer: &Path,
    dir: &TempDir,
    image: &Path,
    runs: usize,
    headless: bool,
) -> Result<(), String> {
    let viewer = Viewer::start(seerw, seer, image, headless)?;
    viewer.wait_for(|timings| phase(timings, "first_present").is_some())?;

    let mut presents = Vec::new();
    let mut handled = Vec::new();
    let mut decoded = Vec::new();
    for run in 1..=runs {
        let file = dir.join(format!("open-{run}.jpg"));
        std::fs::copy(image, &file).map_err(|error| error.to_string())?;
        let started = viewer.age_ms()?;
        let output = viewer.seer([OsStr::new("open"), file.as_os_str()])?;
        if !output.status.success() {
            return Err(format!(
                "seer open exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let timings = viewer.wait_for(|timings| {
            phase(timings, "navigation_presented").is_some_and(|ms| ms >= started)
        })?;
        let presented = phase(&timings, "navigation_presented").unwrap_or(started) - started;
        let navigated =
            phase(&timings, "navigated").ok_or("the viewer reported no navigated")? - started;
        println!("run {run:>2}: {presented:6.1} ms");
        presents.push(presented);
        handled.push(navigated);
        if let Some(ms) = phase(&timings, "navigation_decoded") {
            decoded.push(ms - started);
        }
    }
    viewer.quit()?;

    println!(
        "navigation_presented: median {:.1} ms, worst {:.1} ms",
        median(&mut presents),
        worst(&presents)
    );
    println!("median of each step, in ms from the start of `seer open`:");
    println!("  {:6.1}  navigated", median(&mut handled));
    if !decoded.is_empty() {
        println!("  {:6.1}  navigation_decoded", median(&mut decoded));
    }
    println!("  {:6.1}  navigation_presented", median(&mut presents));
    Ok(())
}

/// The time of the phase called `name`, if it was reached.
fn phase(timings: &Timings, name: &str) -> Option<f64> {
    timings
        .phases
        .iter()
        .find(|phase| phase.name == name)
        .map(|phase| phase.ms)
}

/// A viewer on an instance of its own, driven with `seer` and ended with
/// `seer quit` when dropped.
struct Viewer {
    instance: String,
    seer: PathBuf,
    process: Child,
}

impl Viewer {
    fn start(seerw: &Path, seer: &Path, image: &Path, headless: bool) -> Result<Self, String> {
        let instance = unique_instance();
        let mut command = Command::new(seerw);
        command.arg("--instance").arg(&instance);
        if headless {
            command.arg("--headless");
        }
        let process = command
            .arg(image)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot run {}: {error}", seerw.display()))?;
        let name = InstanceName::new(&instance).map_err(|error| error.to_string())?;
        let mut viewer = Viewer {
            instance,
            seer: seer.to_owned(),
            process,
        };
        client::connect_to_started(&name, &mut viewer.process, VIEWER_TIMEOUT)
            .map_err(|error| format!("the viewer did not start: {error}"))?;
        Ok(viewer)
    }

    /// Runs `seer --instance <this viewer's> <args>`.
    fn seer<'a>(&self, args: impl IntoIterator<Item = &'a OsStr>) -> Result<Output, String> {
        Command::new(&self.seer)
            .args(["--instance", &self.instance])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run {}: {error}", self.seer.display()))
    }

    /// The milliseconds from the viewer's creation to now, the clock of its
    /// timings.
    fn age_ms(&self) -> Result<f64, String> {
        process_age_ms(HANDLE(self.process.as_raw_handle()))
            .ok_or_else(|| "cannot read when the viewer started".to_owned())
    }

    /// Polls `seer debug timings` until `done` is true of them, and returns
    /// them.
    fn wait_for(&self, done: impl Fn(&Timings) -> bool) -> Result<Timings, String> {
        let deadline = Instant::now() + VIEWER_TIMEOUT;
        loop {
            let output = self.seer(["--json", "debug", "timings"].map(OsStr::new))?;
            let reply: Reply = serde_json::from_slice(&output.stdout)
                .map_err(|error| format!("seer debug timings printed no timings: {error}"))?;
            let Reply::Timings(timings) = reply else {
                return Err(format!("seer debug timings replied {reply:?}"));
            };
            if done(&timings) {
                return Ok(timings);
            }
            if Instant::now() >= deadline {
                return Err(format!("gave up waiting: {timings:?}"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Ends the viewer with `seer quit` and waits for it to exit.
    fn quit(mut self) -> Result<(), String> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), String> {
        if self.process.try_wait().ok().flatten().is_some() {
            return Ok(());
        }
        self.seer([OsStr::new("quit")])?;
        let deadline = Instant::now() + VIEWER_TIMEOUT;
        while Instant::now() < deadline {
            if self.process.try_wait().ok().flatten().is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("the viewer did not exit".to_owned())
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
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

/// The release build of `name`, beside the directory this example runs from.
fn release_exe(name: &str) -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = exe
        .parent()
        .and_then(|examples| examples.parent())
        .map(|dir| dir.join(name))
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
