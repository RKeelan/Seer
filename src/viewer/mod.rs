//! The viewer, `seerw.exe`: the window, and the machinery around it.
//!
//! - [`args`]: the command line.
//! - [`window`]: the window class and procedure.
//! - [`theme`]: light and dark.
//! - [`watch`]: reloading images that change on disk.
//! - [`timing`]: start-up timing.
//! - [`log`]: the log file and panic hook.
//!
//! Start-up is arranged so the first frame appears as soon as possible:
//!
//! - The decode worker starts before the window exists, decoding the image at
//!   the size it will be shown.
//! - Meanwhile the main thread creates the window and a WARP device, which
//!   takes a few milliseconds, then starts another thread creating the
//!   hardware Direct3D device, which means loading the graphics driver and can
//!   take longer than everything else together.
//! - The main thread draws the first frame on WARP while the window is still
//!   hidden, and then shows the window, so it never appears blank.
//! - When the hardware device is ready, the frame is drawn there and replaces
//!   WARP's in one composition commit. The full-resolution decode follows on
//!   the worker and replaces the preview.
//!
//! A headless viewer stays on WARP, so it draws the same pixels on every
//! machine.

pub mod args;
pub mod log;
pub mod theme;
pub mod timing;
pub mod watch;
pub mod window;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

use self::args::{RunArgs, ViewerArgs};
use self::timing::Timings;
use self::window::{Handler, WM_DECODED, WM_DEVICE_READY, Window};
use crate::decode::wic::Decoded;
use crate::decode::worker::{Job, Output, Worker};
use crate::error::{Error, Result};
use crate::ipc::instance::InstanceName;
use crate::model::ImageId;
use crate::model::app::DecodeTarget;
use crate::model::geometry::{PixelSize, Rect, Transform};
use crate::render::device::{Composition, Device, SwapChain, hardware_device};
use crate::render::draw::{Bitmaps, Renderer};
use crate::render::scene::{Align, Colour, Interpolation, Item, Scene};

/// The colour behind the image.
const BACKGROUND: Colour = Colour::rgb(32, 32, 32);

/// The colour of messages drawn in the window.
const MESSAGE: Colour = Colour::rgb(220, 220, 220);

/// The one image shown.
const IMAGE: ImageId = ImageId(1);

/// The entry point of `seerw.exe`.
pub fn main() -> ExitCode {
    ExitCode::from(run(std::env::args_os().skip(1)))
}

/// Runs the viewer with the arguments that follow the program name, and
/// returns the exit code.
pub fn run<I>(args: I) -> u8
where
    I: IntoIterator<Item = OsString>,
{
    match args::parse(args).and_then(|args| match args {
        ViewerArgs::Version => {
            attach_parent_console();
            let _ = writeln!(std::io::stdout(), "{}", crate::version_line());
            Ok(0)
        }
        ViewerArgs::Run(args) => start(&args),
    }) {
        Ok(code) => code,
        Err(error) => {
            attach_parent_console();
            let _ = writeln!(std::io::stderr(), "seerw: {error}");
            error.exit_code()
        }
    }
}

/// Opens the window on the first file, and runs until it is closed. Returns
/// the exit code.
fn start(args: &RunArgs) -> Result<u8> {
    let mut timings = Timings::start();
    timings.mark("main");
    InstanceName::from_environment(args.instance.as_deref())?;
    let placement = window::default_placement();
    let client = PixelSize::new(placement.width, placement.height);

    let hwnd = Arc::new(AtomicIsize::new(0));
    let worker = {
        let hwnd = Arc::clone(&hwnd);
        Worker::spawn(move || window::post(hwnd.load(Ordering::Acquire), WM_DECODED))
    };
    let file = args.files.first();
    if let Some(path) = file {
        worker.submit(Job {
            image: IMAGE,
            path: path.clone(),
            target: DecodeTarget::Fit { within: client },
        });
    }

    let window = Window::create(placement, &title(file.map(AsRef::as_ref)))?;
    hwnd.store(window.hwnd().0 as isize, Ordering::Release);
    timings.mark("window_created");
    let composition = Composition::new(window.hwnd())?;
    let device = Device::warp()?;
    let swap_chain = SwapChain::new(&device, window.client_size())?;
    timings.mark("warp_device_created");
    // Direct3D creates one device at a time, so the hardware device, whose
    // driver takes long to load, is started only once WARP's is made.
    let upgrade = (!args.headless).then(|| Upgrade::spawn(Arc::clone(&hwnd)));

    let mut viewer = Viewer {
        device,
        swap_chain,
        composition,
        renderer: Renderer::default(),
        bitmaps: Bitmaps::new(),
        path: file.cloned(),
        image: None,
        pixels: None,
        error: None,
        worker,
        upgrade,
        timings,
    };
    if let Some(output) = file.and_then(|_| viewer.worker.recv()) {
        viewer.accept(output);
    }
    viewer.present()?;
    viewer.composition.show(&viewer.swap_chain)?;
    viewer.timings.mark("first_present");
    if !args.headless {
        window.show(!args.no_activate);
        viewer.timings.mark("shown");
    }

    let code = viewer.error.as_ref().map_or(0, Error::exit_code);
    if args.exit_after_first_frame {
        attach_parent_console();
        let report = serde_json::to_string(&viewer.timings.report())?;
        let _ = writeln!(std::io::stdout(), "{report}");
        return Ok(code);
    }
    if args.headless {
        // Nothing can reach a headless viewer until it serves the pipe.
        return Ok(code);
    }

    window.set_handler(Box::new(viewer));
    // Anything that finished before the window's handle was known was not
    // announced.
    window::post(window.hwnd().0 as isize, WM_DECODED);
    window::post(window.hwnd().0 as isize, WM_DEVICE_READY);
    window::run_message_loop();
    Ok(0)
}

/// The window's title: the file's name, or the program's.
fn title(file: Option<&Path>) -> String {
    file.and_then(Path::file_name).map_or_else(
        || "Seer".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// A hardware Direct3D device being created on its own thread.
struct Upgrade {
    device: Receiver<(Result<ID3D11Device>, Instant)>,
    thread: Option<JoinHandle<()>>,
}

impl Upgrade {
    /// Starts creating the device, and posts [`WM_DEVICE_READY`] to the
    /// window whose handle is in `hwnd` when it is done.
    fn spawn(hwnd: Arc<AtomicIsize>) -> Self {
        let (send, device) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("device".into())
            .spawn(move || {
                let _ = send.send((hardware_device(), Instant::now()));
                window::post(hwnd.load(Ordering::Acquire), WM_DEVICE_READY);
            })
            .ok();
        Upgrade { device, thread }
    }
}

impl Drop for Upgrade {
    /// Waits for the thread, which may be loading the graphics driver: a
    /// process that exits while another thread is loading a library can hang.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The shown image's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    /// The full size, after orientation.
    size: PixelSize,
    /// The size of the bitmap drawn.
    bitmap_size: PixelSize,
}

/// Everything the window needs to draw.
struct Viewer {
    device: Device,
    swap_chain: SwapChain,
    composition: Composition,
    renderer: Renderer,
    bitmaps: Bitmaps,
    /// The file shown.
    path: Option<PathBuf>,
    image: Option<Shown>,
    /// The shown image's pixels, kept until the move to the hardware device
    /// so they can be uploaded there.
    pixels: Option<Decoded>,
    /// Why the image could not be shown.
    error: Option<Error>,
    worker: Worker,
    upgrade: Option<Upgrade>,
    timings: Timings,
}

impl Viewer {
    /// Takes a decode's output: shows the bitmap if it is better than the
    /// one shown, or the error if there is nothing to show.
    fn accept(&mut self, output: Output) {
        let preview = output.target != DecodeTarget::Full;
        let (started, finished) = if preview {
            ("decode_started", "preview_decoded")
        } else {
            ("full_decode_started", "full_decoded")
        };
        self.timings.record(started, output.started);
        self.timings.record(finished, output.finished);
        let decoded = match output.result {
            Ok(decoded) => decoded,
            Err(error) => {
                if self.image.is_none() {
                    self.error = Some(error);
                }
                return;
            }
        };
        if self
            .image
            .is_some_and(|shown| shown.bitmap_size.width >= decoded.bitmap_size.width)
        {
            return;
        }
        let bitmap = match self.device.create_bitmap(&decoded) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                self.error.get_or_insert(error);
                return;
            }
        };
        self.bitmaps.insert(output.image, bitmap);
        self.image = Some(Shown {
            size: decoded.size,
            bitmap_size: decoded.bitmap_size,
        });
        self.error = None;
        if preview
            && decoded.bitmap_size != decoded.size
            && let Some(path) = &self.path
        {
            self.worker.submit(Job {
                image: output.image,
                path: path.clone(),
                target: DecodeTarget::Full,
            });
        }
        if self.upgrade.is_some() {
            self.pixels = Some(decoded);
        }
    }

    /// Draws the frame and presents it.
    fn present(&mut self) -> Result<()> {
        let scene = self.scene();
        draw(
            &self.device,
            &self.swap_chain,
            &mut self.renderer,
            &scene,
            &self.bitmaps,
        )
    }

    fn scene(&self) -> Scene {
        scene(
            self.swap_chain.size(),
            self.image.map(|shown| shown.size),
            self.error.as_ref(),
        )
    }

    /// Moves to the hardware device, if it is ready: uploads the image,
    /// draws the frame there, and shows it in place of WARP's.
    fn upgrade(&mut self) -> Result<()> {
        let Some(upgrade) = self.upgrade.take() else {
            return Ok(());
        };
        let (d3d, ready) = match upgrade.device.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => {
                self.upgrade = Some(upgrade);
                return Ok(());
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
        };
        drop(upgrade);
        let pixels = self.pixels.take();
        self.timings.record("hardware_device_created", ready);
        let device = Device::from_hardware(d3d?)?;
        let swap_chain = SwapChain::new(&device, self.swap_chain.size())?;
        let mut bitmaps = Bitmaps::new();
        if let Some(pixels) = pixels {
            bitmaps.insert(IMAGE, device.create_bitmap(&pixels)?);
        }
        let mut renderer = Renderer::default();
        draw(&device, &swap_chain, &mut renderer, &self.scene(), &bitmaps)?;
        self.composition.show(&swap_chain)?;
        self.device = device;
        self.swap_chain = swap_chain;
        self.renderer = renderer;
        self.bitmaps = bitmaps;
        self.timings.mark("hardware_shown");
        Ok(())
    }
}

/// Draws `scene` to `swap_chain`'s back buffer through `device` and presents
/// it.
fn draw(
    device: &Device,
    swap_chain: &SwapChain,
    renderer: &mut Renderer,
    scene: &Scene,
    bitmaps: &Bitmaps,
) -> Result<()> {
    swap_chain.target(device)?;
    let drawn = renderer.draw(device.context(), scene, bitmaps);
    let presented = swap_chain.present(device);
    drawn.and(presented)
}

impl Handler for Viewer {
    fn paint(&mut self) {
        let _ = self.present();
    }

    fn resize(&mut self, size: PixelSize) {
        if self.swap_chain.resize(size).is_ok() {
            let _ = self.present();
        }
    }

    fn decoded(&mut self) {
        let mut any = false;
        while let Some(output) = self.worker.try_recv() {
            self.accept(output);
            any = true;
        }
        if any {
            let _ = self.present();
        }
    }

    fn device_ready(&mut self) {
        // On failure the viewer stays on WARP, which draws the same frames
        // more slowly.
        let _ = self.upgrade();
    }
}

/// What to draw: the image fitted to the client area, or why it can't be
/// shown.
fn scene(client: PixelSize, image: Option<PixelSize>, error: Option<&Error>) -> Scene {
    let whole = Rect::new(0.0, 0.0, f64::from(client.width), f64::from(client.height));
    let mut items = Vec::new();
    if let Some(size) = image {
        items.push(Item::Image {
            image: IMAGE,
            size,
            transform: fit(size, client),
            clip: whole,
            interpolation: Interpolation::Smooth,
            opacity: 1.0,
        });
    } else if let Some(error) = error {
        items.push(Item::Text {
            text: error.message.clone(),
            rect: Rect::new(16.0, 0.0, (whole.width - 32.0).max(0.0), whole.height),
            colour: MESSAGE,
            size: 16.0,
            align: Align::Centre,
            bold: false,
        });
    }
    Scene {
        size: client,
        background: BACKGROUND,
        items,
    }
}

/// Maps an image of `image` pixels into the middle of a client area of
/// `client` pixels, as large as fits without enlarging it.
fn fit(image: PixelSize, client: PixelSize) -> Transform {
    if image.width == 0 || image.height == 0 {
        return Transform::IDENTITY;
    }
    let (iw, ih) = (f64::from(image.width), f64::from(image.height));
    let (cw, ch) = (f64::from(client.width), f64::from(client.height));
    let scale = (cw / iw).min(ch / ih).min(1.0);
    let dx = ((cw - iw * scale) / 2.0).round();
    let dy = ((ch - ih * scale) / 2.0).round();
    Transform::scale(scale, scale).then(&Transform::translation(dx, dy))
}

/// Lets a GUI-subsystem process print to the console it was started from.
///
/// Windows gives such a process no standard handles unless its parent
/// redirected them. When neither stdout nor stderr is set, this attaches to
/// the parent's console, if it has one, so `seerw --version` typed at a prompt
/// prints there.
fn attach_parent_console() {
    let unset = |which| {
        // SAFETY: GetStdHandle only reads the process's standard handle table.
        unsafe { GetStdHandle(which) }.map_or(true, |handle| handle.is_invalid())
    };
    if unset(STD_OUTPUT_HANDLE) && unset(STD_ERROR_HANDLE) {
        // SAFETY: AttachConsole has no pointer arguments. It fails harmlessly
        // when the parent has no console or this process already has one.
        let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::model::geometry::Point;

    fn strs(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn version_and_usage_errors_set_the_exit_code() {
        assert_eq!(run(strs(&["--version"])), 0);
        assert_eq!(run(strs(&["--frobnicate"])), 2);
        assert_eq!(run(strs(&["--instance", "no spaces"])), 2);
    }

    #[test]
    fn an_empty_headless_viewer_starts_and_exits() {
        assert_eq!(run(strs(&["--headless"])), 0);
    }

    #[test]
    fn the_viewer_moves_to_the_hardware_device_without_a_blank_frame() {
        use crate::model::geometry::PixelRect;
        use crate::testutil::{Format, Pixels, Rgba, TempDir, write_image};
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(640, 480, Rgba::GREEN), Format::Png).unwrap();
        let window = Window::create(PixelRect::new(0, 0, 400, 300), "upgrade").unwrap();
        let hwnd = Arc::new(AtomicIsize::new(window.hwnd().0 as isize));
        let device = Device::warp().unwrap();
        let mut viewer = Viewer {
            swap_chain: SwapChain::new(&device, window.client_size()).unwrap(),
            device,
            composition: Composition::new(window.hwnd()).unwrap(),
            renderer: Renderer::default(),
            bitmaps: Bitmaps::new(),
            path: Some(path.clone()),
            image: None,
            pixels: None,
            error: None,
            worker: Worker::spawn(|| {}),
            upgrade: Some(Upgrade::spawn(hwnd)),
            timings: Timings::start(),
        };
        viewer.worker.submit(Job {
            image: IMAGE,
            path,
            target: DecodeTarget::Fit {
                within: PixelSize::new(400, 300),
            },
        });
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert_eq!(viewer.image.unwrap().bitmap_size, PixelSize::new(400, 300));
        assert!(viewer.pixels.is_some());
        viewer.paint();
        // The full decode follows the preview.
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert_eq!(viewer.image.unwrap().bitmap_size, PixelSize::new(640, 480));
        viewer.resize(PixelSize::new(200, 100));
        assert_eq!(viewer.swap_chain.size(), PixelSize::new(200, 100));
        // Wait for the device thread, then move to its device.
        viewer
            .upgrade
            .as_mut()
            .unwrap()
            .thread
            .take()
            .unwrap()
            .join()
            .unwrap();
        viewer.device_ready();
        assert!(viewer.upgrade.is_none() && viewer.pixels.is_none());
        assert_eq!(viewer.bitmaps.len(), 1);
        viewer.decoded();
        viewer.device_ready();
    }

    #[test]
    fn fitting_centres_without_enlarging() {
        let client = PixelSize::new(800, 600);
        let large = fit(PixelSize::new(1600, 600), client);
        assert_eq!(large.apply(Point::new(0.0, 0.0)), Point::new(0.0, 150.0));
        assert_eq!(
            large.apply(Point::new(1600.0, 600.0)),
            Point::new(800.0, 450.0)
        );
        let small = fit(PixelSize::new(100, 50), client);
        assert_eq!(small.apply(Point::new(0.0, 0.0)), Point::new(350.0, 275.0));
        assert_eq!(
            small.apply(Point::new(100.0, 50.0)),
            Point::new(450.0, 325.0)
        );
        assert_eq!(fit(PixelSize::new(0, 5), client), Transform::IDENTITY);
    }

    #[test]
    fn scenes_show_the_image_or_the_error() {
        let client = PixelSize::new(100, 100);
        let shown = scene(client, Some(PixelSize::new(10, 10)), None);
        assert!(matches!(
            shown.items[..],
            [Item::Image { image: IMAGE, .. }]
        ));
        assert_eq!(shown.background, BACKGROUND);
        let error = Error::new(ErrorCode::FileNotFound, "gone");
        let failed = scene(client, None, Some(&error));
        assert!(matches!(&failed.items[..], [Item::Text { text, .. }] if text == "gone"));
        assert!(scene(client, None, None).items.is_empty());
    }

    #[test]
    fn titles_name_the_file() {
        assert_eq!(title(Some(Path::new(r"C:\pictures\cat.jpg"))), "cat.jpg");
        assert_eq!(title(None), "Seer");
    }
}
