//! The viewer, `seerw.exe`: the window, and the machinery around it.
//!
//! - [`args`]: the command line.
//! - [`window`]: the window class and procedure.
//! - [`note`]: the note field.
//! - [`theme`]: light and dark.
//! - [`watch`]: reloading images that change on disk.
//! - [`timing`]: start-up timing.
//! - [`log`]: the log file and panic hook.
//!
//! The viewer is the [`App`] state machine wired to Win32. The window
//! procedure turns messages into [`Input`]; the pipe server's requests and
//! the decode worker's results arrive as [`Input`] too, posted to the UI
//! thread, which never blocks on a pipe. The viewer carries out the
//! [`Effect`]s `App` returns and draws [`App::scene`].
//!
//! The first viewer on an instance takes the instance's mutex. A later one
//! finds the mutex taken, hands its files to the first over the pipe, asking
//! it to come to the foreground, and exits.
//!
//! Start-up is arranged so the first frame appears as soon as possible:
//!
//! - The decode worker starts before the window exists, decoding the first
//!   image at the size it will be shown.
//! - Meanwhile the main thread creates the window and a WARP device, which
//!   takes a few milliseconds, then starts another thread creating the
//!   hardware Direct3D device, which means loading the graphics driver and can
//!   take longer than everything else together.
//! - The main thread draws the first frame on WARP while the window is still
//!   hidden, and then shows the window, so it never appears blank.
//! - Only then do the pipe server and the file watcher start.
//! - When the hardware device is ready, the frame is drawn there and replaces
//!   WARP's in one composition commit. The full-resolution decode follows on
//!   the worker and replaces the preview.
//!
//! A viewer started with no files, as `seer open` starts one before sending
//! the files over the pipe, stays hidden until the first image it is sent has
//! been drawn, or until [`EMPTY_SHOW_DELAY_MS`] passes with nothing to show.
//!
//! A headless viewer never shows its window and stays on WARP, so it draws
//! the same pixels on every machine.

pub mod args;
pub mod log;
pub mod note;
pub mod theme;
pub mod timing;
pub mod watch;
pub mod window;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

use self::args::{RunArgs, ViewerArgs};
use self::note::NoteField;
use self::timing::Timings;
use self::watch::{Reloads, Watcher};
use self::window::{Handler, WM_DECODED, WM_DEVICE_READY, WM_FILES_CHANGED, WM_REQUEST, Window};
use crate::decode::wic::Decoded;
use crate::decode::worker::{Job, Output, Worker};
use crate::error::{Error, ErrorCode, Result};
use crate::ipc::client;
use crate::ipc::instance::{InstanceMutex, InstanceName};
use crate::ipc::server::{self, Responder, Server};
use crate::model::ImageId;
use crate::model::app::{App, Config, Cursor, DecodeTarget, Effect, Input, RequestId};
use crate::model::geometry::{PixelSize, Rect};
use crate::model::set::DiskFiles;
use crate::protocol::{OpenFile, Reply, Request, Response};
use crate::render::device::{Composition, Device, SwapChain, hardware_device};
use crate::render::draw::{Bitmaps, Renderer};
use crate::render::offscreen;
use crate::render::scene::Scene;

/// The id of the request the viewer makes of itself to open the files on its
/// command line. The pipe server numbers its requests from 1.
const COMMAND_LINE: RequestId = RequestId(0);

/// The timer behind [`Effect::WakeAfter`].
const WAKE_TIMER: usize = 1;

/// The timer that shows a window started with no files that has been sent
/// nothing to show.
const SHOW_TIMER: usize = 2;

/// The timer that reloads changed files once they are due.
const RELOAD_TIMER: usize = 3;

/// How long a viewer started with no files waits for an image before showing
/// its window empty.
pub const EMPTY_SHOW_DELAY_MS: u64 = 1000;

/// How long a viewer that is exiting waits for its last replies to reach
/// their clients.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Whether this process owns the instance or handed its files to the one that
/// does.
enum Ownership {
    Owner(InstanceMutex),
    /// The files went to the running viewer; this is the exit code.
    HandedOff(u8),
}

/// Opens the window on the files, and runs until it is closed or told to
/// quit. Returns the exit code.
fn start(args: &RunArgs) -> Result<u8> {
    let mut timings = Timings::start();
    timings.mark("main");
    let instance = InstanceName::from_environment(args.instance.as_deref())?;
    let files = args
        .files
        .iter()
        .map(|path| std::path::absolute(path).map_err(Error::from))
        .collect::<Result<Vec<_>>>()?;
    let _mutex = match take_instance(&instance, &files)? {
        Ownership::Owner(mutex) => mutex,
        Ownership::HandedOff(code) => return Ok(code),
    };

    let placement = window::default_placement();
    let hwnd = Arc::new(AtomicIsize::new(0));
    let worker = {
        let hwnd = Arc::clone(&hwnd);
        Worker::spawn(move || window::post(hwnd.load(Ordering::Acquire), WM_DECODED))
    };
    // A headless viewer is always dark, so its screenshots do not depend on
    // the machine's setting.
    let dark = args.headless || theme::apps_dark();
    let config = Config {
        headless: args.headless,
        size: PixelSize::new(placement.width, placement.height),
        dpi: window::system_dpi(),
        dark,
    };
    let mut app = App::new(config, Box::new(DiskFiles), Instant::now());
    // The decodes start at once; everything else waits for the window.
    let mut early = Vec::new();
    if !files.is_empty() {
        let request = open_request(&files, !args.no_activate);
        let input = Input::Request {
            id: COMMAND_LINE,
            request,
        };
        for effect in app.handle(input, Instant::now()) {
            match effect {
                Effect::Decode {
                    image,
                    path,
                    target,
                } => worker.submit(Job {
                    image,
                    path,
                    target,
                }),
                other => early.push(other),
            }
        }
    }

    let window = Window::create(placement, &app.title())?;
    theme::set_title_bar(window.hwnd(), dark);
    hwnd.store(window.hwnd().0 as isize, Ordering::Release);
    timings.mark("window_created");
    let composition = Composition::new(window.hwnd())?;
    let device = Device::warp()?;
    let swap_chain = SwapChain::new(&device, window.client_size())?;
    timings.mark("warp_device_created");
    // Direct3D creates one device at a time, so the hardware device, whose
    // driver takes long to load, is started only once WARP's is made.
    let upgrade = (!args.headless).then(|| Upgrade::spawn(Arc::clone(&hwnd)));

    let (events, received) = mpsc::channel();
    let mut viewer = Viewer::new(
        window.hwnd(),
        app,
        Graphics {
            device,
            swap_chain,
            composition,
        },
        worker,
        upgrade,
        timings,
        received,
    );
    if !args.headless {
        viewer.visibility = Visibility::Hidden {
            activate: !args.no_activate,
            waited: !files.is_empty(),
        };
    }
    viewer.perform(early);
    while let Some(image) = viewer.awaited() {
        let Some(output) = viewer.worker.recv() else {
            break;
        };
        let first = output.image == image;
        viewer.accept(output);
        if first {
            break;
        }
    }
    viewer.present()?;
    viewer
        .graphics
        .composition
        .show(&viewer.graphics.swap_chain)?;
    viewer.timings.mark("first_present");
    viewer.show_if_ready();

    if args.exit_after_first_frame {
        attach_parent_console();
        let report = serde_json::to_string(&viewer.timings.report())?;
        let _ = writeln!(std::io::stdout(), "{report}");
        return Ok(viewer.first_error.as_ref().map_or(0, Error::exit_code));
    }
    viewer.watcher.start();
    if matches!(viewer.visibility, Visibility::Hidden { .. }) {
        window::set_timer(window.hwnd(), SHOW_TIMER, EMPTY_SHOW_DELAY_MS);
    }

    let server = Server::start(
        &instance,
        Pipe {
            events,
            hwnd: Arc::clone(&hwnd),
        },
    )?;
    window.set_handler(Box::new(viewer));
    // Anything that finished before the window's handle was known, or
    // before the handler was in place, was not announced.
    for message in [WM_DECODED, WM_DEVICE_READY, WM_REQUEST] {
        window::post(window.hwnd().0 as isize, message);
    }
    window::run_message_loop();
    // Dropping the viewer answers any request it still holds with an error.
    drop(window.take_handler());
    server.stop(REPLY_TIMEOUT);
    Ok(0)
}

/// Takes the instance's mutex, or hands the files to the viewer that holds
/// it.
///
/// A viewer that holds the mutex may not serve its pipe yet, and one that is
/// exiting may let go of the mutex without answering, so this tries both
/// again until one succeeds or [`client::LAUNCH_TIMEOUT`] passes.
fn take_instance(instance: &InstanceName, files: &[PathBuf]) -> Result<Ownership> {
    let deadline = Instant::now() + client::LAUNCH_TIMEOUT;
    loop {
        if let Some(mutex) = InstanceMutex::acquire(instance)? {
            return Ok(Ownership::Owner(mutex));
        }
        if files.is_empty() {
            return Ok(Ownership::HandedOff(0));
        }
        match hand_off(instance, files) {
            Err(error) if error.code == ErrorCode::NoViewer && Instant::now() < deadline => {}
            result => return result.map(Ownership::HandedOff),
        }
    }
}

/// Sends the files to the running viewer, letting it come to the foreground,
/// and returns the exit code: 4 if any could not be opened.
fn hand_off(instance: &InstanceName, files: &[PathBuf]) -> Result<u8> {
    let reply = client::connect(instance)?.send(&open_request(files, true))?;
    Ok(match reply {
        Reply::Opened(opened) => opened
            .failed
            .first()
            .map_or(0, |failure| failure.error.exit_code()),
        _ => 0,
    })
}

fn open_request(files: &[PathBuf], activate: bool) -> Request {
    Request::Open {
        files: files
            .iter()
            .map(|path| OpenFile {
                path: path.clone(),
                caption: None,
            })
            .collect(),
        group: None,
        compare: false,
        activate,
    }
}

/// What the pipe server hands the UI thread.
enum PipeEvent {
    Request(Request, Responder),
    Disconnected(RequestId),
}

/// The pipe server's handler: posts each request to the UI thread.
struct Pipe {
    events: Sender<PipeEvent>,
    hwnd: Arc<AtomicIsize>,
}

impl Pipe {
    fn post(&self, event: PipeEvent) {
        // If the viewer has gone, the event is dropped, and with it the
        // responder, which answers with an error.
        if self.events.send(event).is_ok() {
            window::post(self.hwnd.load(Ordering::Acquire), WM_REQUEST);
        }
    }
}

impl server::Handler for Pipe {
    fn request(&self, request: Request, responder: Responder) {
        self.post(PipeEvent::Request(request, responder));
    }

    fn disconnected(&self, id: RequestId) {
        self.post(PipeEvent::Disconnected(id));
    }
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

/// Whether the window is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visibility {
    /// Headless: never shown.
    Never,
    /// Not shown yet.
    Hidden {
        /// Whether to activate the window when it is shown.
        activate: bool,
        /// Whether to show it even with nothing in the set.
        waited: bool,
    },
    Shown,
}

/// What the viewer draws with.
struct Graphics {
    device: Device,
    swap_chain: SwapChain,
    composition: Composition,
}

/// The window's handler: runs [`App`] and carries out its effects.
struct Viewer {
    hwnd: HWND,
    app: App,
    graphics: Graphics,
    renderer: Renderer,
    bitmaps: Bitmaps,
    /// The size of each image's bitmap in `bitmaps`.
    bitmap_sizes: HashMap<ImageId, PixelSize>,
    /// The decoded pixels behind `bitmaps`, kept until the move to the
    /// hardware device so they can be uploaded there.
    pixels: HashMap<ImageId, Decoded>,
    /// The images whose decode failed.
    failed: HashSet<ImageId>,
    worker: Worker,
    upgrade: Option<Upgrade>,
    timings: Timings,
    /// Requests from the pipe server.
    events: Receiver<PipeEvent>,
    /// The way back to each client whose request is not answered yet.
    responders: HashMap<RequestId, Responder>,
    visibility: Visibility,
    cursor: Cursor,
    /// The note field, made the first time it is shown.
    note: Option<NoteField>,
    /// Whether what is shown has changed since it was last presented.
    dirty: bool,
    /// The first reason a file on the command line could not be shown.
    first_error: Option<Error>,
    /// Watches the open images' files.
    watcher: Watcher,
    /// The changed files waiting to be reloaded.
    reloads: Reloads,
}

impl Viewer {
    fn new(
        hwnd: HWND,
        app: App,
        graphics: Graphics,
        worker: Worker,
        upgrade: Option<Upgrade>,
        timings: Timings,
        events: Receiver<PipeEvent>,
    ) -> Self {
        let posted_to = hwnd.0 as isize;
        Viewer {
            hwnd,
            app,
            graphics,
            renderer: Renderer::default(),
            bitmaps: Bitmaps::new(),
            bitmap_sizes: HashMap::new(),
            pixels: HashMap::new(),
            failed: HashSet::new(),
            worker,
            upgrade,
            timings,
            events,
            responders: HashMap::new(),
            visibility: Visibility::Never,
            cursor: Cursor::Arrow,
            note: None,
            dirty: false,
            first_error: None,
            watcher: Watcher::new(move || window::post(posted_to, WM_FILES_CHANGED)),
            reloads: Reloads::new(),
        }
    }

    /// Hands an input to the app, carries out the effects, and presents the
    /// frame if it changed.
    fn handle(&mut self, input: Input) {
        let effects = self.app.handle(input, Instant::now());
        self.perform(effects);
    }

    fn perform(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Redraw => self.dirty = true,
                Effect::SetTitle { title } => window::set_title(self.hwnd, &title),
                Effect::Reply { id, response } => self.reply(id, response),
                Effect::Screenshot { id, path } => {
                    let response = self.screenshot(path).into();
                    self.reply(id, response);
                }
                Effect::ReportTimings { id } => {
                    let response = Response::Ok(Reply::Timings(self.timings.report()));
                    self.reply(id, response);
                }
                Effect::Decode {
                    image,
                    path,
                    target,
                } => self.worker.submit(Job {
                    image,
                    path,
                    target,
                }),
                Effect::CancelDecode { image } => {
                    self.bitmaps.remove(&image);
                    self.bitmap_sizes.remove(&image);
                    self.pixels.remove(&image);
                    self.failed.remove(&image);
                }
                Effect::SetCursor { cursor } => self.cursor = cursor,
                Effect::Activate => match &mut self.visibility {
                    Visibility::Hidden { activate, .. } => *activate = true,
                    Visibility::Shown => window::activate(self.hwnd),
                    Visibility::Never => {}
                },
                Effect::FlashTaskbar => {
                    if self.visibility == Visibility::Shown {
                        window::flash(self.hwnd);
                    }
                }
                Effect::WakeAfter { ms } => window::set_timer(self.hwnd, WAKE_TIMER, ms),
                Effect::Quit => window::quit(),
                Effect::ShowNoteField { rect, text } => self.show_note(rect, &text),
                Effect::HideNoteField => {
                    if let Some(note) = &self.note {
                        note.hide();
                    }
                }
                Effect::Watch { image, path } => self.watcher.watch(image, path),
                Effect::Unwatch { image } => {
                    self.watcher.unwatch(image);
                    self.reloads.forget(image);
                }
            }
        }
    }

    /// Shows the note field, giving it the keyboard focus when it appears,
    /// unless the viewer is headless.
    fn show_note(&mut self, rect: Rect, text: &str) {
        if self.note.is_none() {
            self.note = NoteField::create(self.hwnd).ok();
        }
        if let Some(note) = &mut self.note
            && note.show(rect, text)
            && self.visibility != Visibility::Never
        {
            note.focus();
        }
    }

    /// Presents the frame if it changed, and shows the window if it is ready
    /// to be seen.
    fn finish(&mut self) {
        if self.dirty {
            let _ = self.present();
        }
        self.show_if_ready();
    }

    fn reply(&mut self, id: RequestId, response: Response) {
        if id == COMMAND_LINE {
            if let Response::Ok(Reply::Opened(opened)) = &response
                && let Some(failure) = opened.failed.first()
            {
                self.first_error.get_or_insert(failure.error.clone());
            }
        } else if let Some(responder) = self.responders.remove(&id) {
            responder.respond(response);
        }
    }

    /// Writes the frame as the user would see it to a PNG.
    fn screenshot(&mut self, path: PathBuf) -> Result<Reply> {
        let capture = offscreen::render(
            &self.graphics.device,
            &mut self.renderer,
            &self.app.scene(),
            &self.bitmaps,
        )?;
        offscreen::write_png(&path, &capture)?;
        Ok(Reply::Screenshot {
            path,
            size: capture.size,
        })
    }

    /// The selected image, if the first frame should wait for its decode.
    fn awaited(&self) -> Option<ImageId> {
        self.app
            .set()
            .selected()
            .filter(|image| !self.bitmaps.contains_key(image) && !self.failed.contains(image))
    }

    /// Shows a hidden window once there is something to see: the selected
    /// image, or why it can't be shown, or, after a wait, an empty set.
    fn show_if_ready(&mut self) {
        let Visibility::Hidden { activate, waited } = self.visibility else {
            return;
        };
        let ready = match self.app.set().selected() {
            Some(_) => self.awaited().is_none(),
            None => waited,
        };
        if ready {
            if self.dirty {
                let _ = self.present();
            }
            window::show(self.hwnd, activate);
            self.visibility = Visibility::Shown;
            self.timings.mark("shown");
        }
    }

    /// Takes a decode's output: keeps the bitmap if it is better than the one
    /// held, or is the full image read again after its file changed, and
    /// tells the app.
    ///
    /// A reload that fails while an older bitmap is held is tried again
    /// later, and the app is not told, so the old image stays on screen.
    fn accept(&mut self, output: Output) {
        let preview = output.target != DecodeTarget::Full;
        let (started, finished) = if preview {
            ("decode_started", "preview_decoded")
        } else {
            ("full_decode_started", "full_decoded")
        };
        self.timings.record_first(started, output.started);
        self.timings.record_first(finished, output.finished);
        let image = output.image;
        if self.app.set().get(image).is_none() {
            return;
        }
        let decoded = match output.result {
            Ok(decoded) => decoded,
            Err(error) => {
                if self.reloads.contains(image) {
                    self.reloads.failed(image, Instant::now());
                    self.schedule_reloads();
                    if self.bitmaps.contains_key(&image) {
                        return;
                    }
                }
                return self.failed(image, error);
            }
        };
        // A preview that is the full image needs no full decode after it.
        let target = if decoded.bitmap_size == decoded.size {
            DecodeTarget::Full
        } else {
            output.target
        };
        let size = decoded.size;
        let reloaded = target == DecodeTarget::Full && self.reloads.contains(image);
        if reloaded {
            self.reloads.decoded(image);
        }
        if reloaded
            || self
                .bitmap_sizes
                .get(&image)
                .is_none_or(|held| held.width < decoded.bitmap_size.width)
        {
            match self.graphics.device.create_bitmap(&decoded) {
                Ok(bitmap) => {
                    self.bitmaps.insert(image, bitmap);
                    self.bitmap_sizes.insert(image, decoded.bitmap_size);
                    if self.upgrade.is_some() {
                        self.pixels.insert(image, decoded);
                    }
                }
                Err(error) => return self.failed(image, error),
            }
        }
        self.failed.remove(&image);
        self.handle(Input::Decoded {
            image,
            target,
            size,
        });
    }

    /// Reloads the changed files that are due, or marks them missing if
    /// they are gone, and sets the timer for the next.
    fn reload_due(&mut self) {
        for image in self.reloads.due(Instant::now()) {
            let Some(entry) = self.app.set().get(image) else {
                self.reloads.forget(image);
                continue;
            };
            if entry.path.is_file() {
                self.handle(Input::FileChanged { image });
            } else {
                self.reloads.forget(image);
                self.handle(Input::FileMissing { image });
            }
        }
        self.schedule_reloads();
    }

    /// Sets the timer for the next reload that is due, if any.
    fn schedule_reloads(&self) {
        if let Some(due) = self.reloads.next_due() {
            let wait = due.saturating_duration_since(Instant::now());
            // Round up, so the timer never fires before the reload is due.
            let ms = u64::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(u64::MAX);
            window::set_timer(self.hwnd, RELOAD_TIMER, ms);
        }
    }

    fn failed(&mut self, image: ImageId, error: Error) {
        if self.app.set().selected() == Some(image) {
            self.first_error.get_or_insert(error.clone());
        }
        self.failed.insert(image);
        self.handle(Input::DecodeFailed { image, error });
    }

    /// Draws the frame and presents it.
    fn present(&mut self) -> Result<()> {
        self.dirty = false;
        let scene = self.app.scene();
        let graphics = &self.graphics;
        draw(
            &graphics.device,
            &graphics.swap_chain,
            &mut self.renderer,
            &scene,
            &self.bitmaps,
        )
    }

    /// Moves to the hardware device, if it is ready: uploads the images,
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
        let pixels = std::mem::take(&mut self.pixels);
        self.timings.record("hardware_device_created", ready);
        let device = Device::from_hardware(d3d?)?;
        let swap_chain = SwapChain::new(&device, self.graphics.swap_chain.size())?;
        let mut bitmaps = Bitmaps::new();
        for (image, decoded) in &pixels {
            bitmaps.insert(*image, device.create_bitmap(decoded)?);
        }
        let mut renderer = Renderer::default();
        draw(
            &device,
            &swap_chain,
            &mut renderer,
            &self.app.scene(),
            &bitmaps,
        )?;
        self.graphics.composition.show(&swap_chain)?;
        self.graphics.device = device;
        self.graphics.swap_chain = swap_chain;
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
        // A minimised window has no client area; keep the last frame.
        if size.width == 0 || size.height == 0 {
            return;
        }
        let _ = self.graphics.swap_chain.resize(size);
        self.handle(Input::Resize { size });
        self.dirty = true;
        self.finish();
    }

    fn decoded(&mut self) {
        while let Some(output) = self.worker.try_recv() {
            self.accept(output);
        }
        self.finish();
    }

    fn device_ready(&mut self) {
        // On failure the viewer stays on WARP, which draws the same frames
        // more slowly.
        let _ = self.upgrade();
    }

    fn requests(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                PipeEvent::Request(request, responder) => {
                    let id = responder.id();
                    self.responders.insert(id, responder);
                    self.handle(Input::Request { id, request });
                }
                PipeEvent::Disconnected(id) => {
                    self.responders.remove(&id);
                    self.handle(Input::Disconnected { id });
                }
            }
        }
        self.finish();
    }

    fn timer(&mut self, id: usize) {
        match id {
            WAKE_TIMER => self.handle(Input::Tick),
            SHOW_TIMER => {
                if let Visibility::Hidden { waited, .. } = &mut self.visibility {
                    *waited = true;
                }
            }
            RELOAD_TIMER => self.reload_due(),
            _ => {}
        }
        self.finish();
    }

    fn input(&mut self, input: Input) {
        self.handle(input);
        self.finish();
    }

    fn cursor(&mut self) -> Cursor {
        self.cursor
    }

    fn focus(&mut self) {
        if let Some(note) = &self.note
            && note.is_shown()
        {
            note.focus();
        }
    }

    fn files_changed(&mut self) {
        let now = Instant::now();
        for image in self.watcher.changes() {
            self.reloads.changed(image, now);
        }
        self.schedule_reloads();
    }

    fn theme_changed(&mut self) {
        if self.visibility == Visibility::Never {
            return;
        }
        let dark = theme::apps_dark();
        theme::set_title_bar(self.hwnd, dark);
        self.input(Input::Theme { dark });
    }
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
    use std::path::Path;

    use super::*;
    use crate::model::app::{Button, Key, Modifiers};
    use crate::model::geometry::{PixelRect, Point};
    use crate::protocol::{ListReply, LoadState};
    use crate::testutil::{
        Format, Pixels, Rgba, TempDir, read_image, unique_instance, write_image,
    };

    fn strs(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn version_and_usage_errors_set_the_exit_code() {
        assert_eq!(run(strs(&["--version"])), 0);
        assert_eq!(run(strs(&["--frobnicate"])), 2);
        assert_eq!(run(strs(&["--instance", "no spaces"])), 2);
    }

    /// Runs a headless viewer on its own thread, as `seerw --headless` would
    /// run, and returns once it serves its pipe.
    fn headless(instance: &InstanceName, files: &[&Path]) -> JoinHandle<u8> {
        let mut args = strs(&["--headless", "--instance", instance.as_str()]);
        args.extend(files.iter().map(|file| file.as_os_str().to_owned()));
        let thread = std::thread::spawn(move || run(args));
        let deadline = Instant::now() + Duration::from_secs(10);
        while client::connect(instance).is_err() {
            assert!(
                Instant::now() < deadline,
                "the viewer never served its pipe"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        thread
    }

    fn list(instance: &InstanceName) -> ListReply {
        match client::send(instance, &Request::List).unwrap() {
            Reply::List(list) => list,
            other => panic!("unexpected reply {other:?}"),
        }
    }

    fn wait_until_loaded(instance: &InstanceName, count: usize) -> ListReply {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let list = list(instance);
            if list.images.len() == count
                && list
                    .images
                    .iter()
                    .all(|image| matches!(image.state, LoadState::Loaded | LoadState::Failed))
            {
                return list;
            }
            assert!(Instant::now() < deadline, "never loaded: {list:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn graphics(window: &Window) -> Graphics {
        let device = Device::warp().unwrap();
        Graphics {
            swap_chain: SwapChain::new(&device, window.client_size()).unwrap(),
            device,
            composition: Composition::new(window.hwnd()).unwrap(),
        }
    }

    fn app(window: &Window, headless: bool) -> App {
        let config = Config {
            headless,
            size: window.client_size(),
            dpi: 96,
            dark: true,
        };
        App::new(config, Box::new(DiskFiles), Instant::now())
    }

    fn key(key: Key) -> Input {
        Input::Key {
            key,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn a_headless_viewer_serves_requests_until_told_to_quit() {
        let dir = TempDir::new().unwrap();
        let red = dir.join("red.png");
        let blue = dir.join("blue.png");
        write_image(&red, &Pixels::solid(40, 30, Rgba::RED), Format::Png).unwrap();
        write_image(&blue, &Pixels::solid(40, 30, Rgba::BLUE), Format::Png).unwrap();
        let instance = InstanceName::new(&unique_instance()).unwrap();
        let viewer = headless(&instance, &[&red]);

        // A second viewer on the instance hands its file over and exits.
        let second = strs(&["--instance", instance.as_str(), blue.to_str().unwrap()]);
        assert_eq!(run(second), 0);
        let list = wait_until_loaded(&instance, 2);
        assert_eq!(list.selected, Some(list.images[1].id));
        assert_eq!(list.images[0].size, Some(PixelSize::new(40, 30)));

        let input = Request::DebugInput {
            events: vec![
                serde_json::from_str(r#"{"type":"resize","size":{"width":80,"height":60}}"#)
                    .unwrap(),
                serde_json::from_str(r#"{"type":"key","key":"left"}"#).unwrap(),
            ],
        };
        assert_eq!(client::send(&instance, &input).unwrap(), Reply::Done);
        let shot = dir.join("shot.png");
        let reply = client::send(&instance, &Request::Screenshot { path: shot.clone() }).unwrap();
        assert_eq!(
            reply,
            Reply::Screenshot {
                path: shot.clone(),
                size: PixelSize::new(80, 60)
            }
        );
        assert_eq!(read_image(&shot).unwrap().get(40, 30), Rgba::RED);

        let Reply::Timings(timings) = client::send(&instance, &Request::DebugTimings).unwrap()
        else {
            panic!("no timings");
        };
        let names: Vec<&str> = timings.phases.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"first_present"), "{names:?}");
        assert!(!names.contains(&"shown"), "{names:?}");

        let unwritable = dir.join("no such directory").join("shot.png");
        let error = client::send(&instance, &Request::Screenshot { path: unwritable }).unwrap_err();
        assert_ne!(error.code, ErrorCode::NoViewer);

        assert_eq!(
            client::send(&instance, &Request::Quit).unwrap(),
            Reply::Done
        );
        assert_eq!(viewer.join().unwrap(), 0);
        assert!(!instance.is_taken());
    }

    #[test]
    fn an_empty_headless_viewer_starts_and_quits() {
        let instance = InstanceName::new(&unique_instance()).unwrap();
        let viewer = headless(&instance, &[]);
        assert!(list(&instance).images.is_empty());
        // A second viewer with no files leaves the first alone.
        assert_eq!(run(strs(&["--instance", instance.as_str()])), 0);
        assert_eq!(
            client::send(&instance, &Request::Quit).unwrap(),
            Reply::Done
        );
        assert_eq!(viewer.join().unwrap(), 0);
    }

    #[test]
    fn the_viewer_moves_to_the_hardware_device_without_a_blank_frame() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(640, 480, Rgba::GREEN), Format::Png).unwrap();
        let window = Window::create(PixelRect::new(0, 0, 400, 300), "upgrade").unwrap();
        let hwnd = Arc::new(AtomicIsize::new(window.hwnd().0 as isize));
        let (_events, received) = mpsc::channel();
        let mut viewer = Viewer::new(
            window.hwnd(),
            app(&window, false),
            graphics(&window),
            Worker::spawn(|| {}),
            Some(Upgrade::spawn(hwnd)),
            Timings::start(),
            received,
        );
        viewer.handle(Input::Request {
            id: COMMAND_LINE,
            request: open_request(&[path.clone(), dir.join("absent.png")], false),
        });
        assert_eq!(
            viewer.first_error.as_ref().unwrap().code,
            ErrorCode::FileNotFound
        );
        let image = viewer.awaited().unwrap();
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert_eq!(viewer.bitmap_sizes[&image], PixelSize::new(400, 300));
        assert!(viewer.awaited().is_none());
        assert!(viewer.pixels.contains_key(&image));
        viewer.paint();
        // The app asks for the full decode once the preview is shown.
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert_eq!(viewer.bitmap_sizes[&image], PixelSize::new(640, 480));
        viewer.resize(PixelSize::new(200, 100));
        viewer.resize(PixelSize::new(0, 0));
        assert_eq!(viewer.graphics.swap_chain.size(), PixelSize::new(200, 100));
        viewer.input(key(Key::Char('1')));
        assert_eq!(viewer.cursor(), Cursor::Arrow);
        viewer.timer(WAKE_TIMER);
        viewer.timer(SHOW_TIMER);
        viewer.timer(99);
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
        assert!(viewer.upgrade.is_none() && viewer.pixels.is_empty());
        assert_eq!(viewer.bitmaps.len(), 1);
        viewer.decoded();
        viewer.device_ready();
        viewer.requests();
        assert!(window::title(window.hwnd()).starts_with("image.png — 640 × 480"));
        // Closing the image drops its bitmap.
        viewer.input(key(Key::Delete));
        assert!(viewer.bitmaps.is_empty());
        assert_eq!(window::title(window.hwnd()), "Seer");
    }

    #[test]
    fn a_file_that_cannot_be_decoded_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("not an image.png");
        std::fs::write(&path, b"not an image").unwrap();
        let window = Window::create(PixelRect::new(0, 0, 100, 100), "failed").unwrap();
        let (_events, received) = mpsc::channel();
        let mut viewer = Viewer::new(
            window.hwnd(),
            app(&window, true),
            graphics(&window),
            Worker::spawn(|| {}),
            None,
            Timings::start(),
            received,
        );
        viewer.handle(Input::Request {
            id: COMMAND_LINE,
            request: open_request(&[path], true),
        });
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert!(viewer.awaited().is_none());
        assert_eq!(viewer.first_error.as_ref().unwrap().exit_code(), 4);
        viewer.finish();
    }

    /// The open images, as `seer list` reports them.
    fn listed(viewer: &mut Viewer) -> ListReply {
        let id = RequestId(99);
        let effects = viewer.app.handle(
            Input::Request {
                id,
                request: Request::List,
            },
            Instant::now(),
        );
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Reply {
                    response: Response::Ok(Reply::List(list)),
                    ..
                } => Some(list),
                _ => None,
            })
            .expect("a list")
    }

    /// Waits for the watcher to report a change, then for the reload to
    /// come due, and runs it.
    fn reload(viewer: &mut Viewer, image: ImageId) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !viewer.reloads.contains(image) {
            assert!(Instant::now() < deadline, "no change seen");
            std::thread::sleep(Duration::from_millis(10));
            viewer.files_changed();
        }
        while let Some(due) = viewer.reloads.next_due() {
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            viewer.timer(RELOAD_TIMER);
        }
    }

    #[test]
    fn a_failed_reload_keeps_the_old_image_and_tries_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(40, 30, Rgba::BLUE), Format::Png).unwrap();
        let window = Window::create(PixelRect::new(0, 0, 100, 100), "reload").unwrap();
        let (_events, received) = mpsc::channel();
        let mut viewer = Viewer::new(
            window.hwnd(),
            app(&window, true),
            graphics(&window),
            Worker::spawn(|| {}),
            None,
            Timings::start(),
            received,
        );
        viewer.handle(Input::Request {
            id: COMMAND_LINE,
            request: open_request(std::slice::from_ref(&path), false),
        });
        let image = viewer.app.set().selected().unwrap();
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        viewer.watcher.start();

        // A rewrite of the same size replaces the bitmap.
        write_image(&path, &Pixels::solid(40, 30, Rgba::RED), Format::Png).unwrap();
        reload(&mut viewer, image);
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert!(!viewer.reloads.contains(image));
        let shot = dir.join("shot.png");
        viewer.screenshot(shot.clone()).unwrap();
        assert_eq!(read_image(&shot).unwrap().get(50, 50), Rgba::RED);

        // A half-written file is retried, and the old image stays.
        std::fs::write(&path, b"\x89PNG half").unwrap();
        reload(&mut viewer, image);
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert!(viewer.reloads.contains(image));
        assert_eq!(listed(&mut viewer).images[0].state, LoadState::Loaded);
        viewer.screenshot(shot.clone()).unwrap();
        assert_eq!(read_image(&shot).unwrap().get(50, 50), Rgba::RED);

        // Once the file is gone, it is reported missing.
        std::fs::remove_file(&path).unwrap();
        reload(&mut viewer, image);
        assert!(listed(&mut viewer).images[0].missing);
        assert!(!viewer.reloads.contains(image));

        // A file that failed from the start reports a failed reload.
        viewer.reloads.changed(image, Instant::now());
        viewer.bitmaps.clear();
        std::fs::write(&path, b"not an image").unwrap();
        reload(&mut viewer, image);
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        assert_eq!(listed(&mut viewer).images[0].state, LoadState::Failed);

        // Closing the image stops watching it.
        viewer.input(key(Key::Delete));
        assert!(!viewer.watcher.is_watching(&path));
        // A reload due for an image no longer open is dropped.
        viewer.reloads.changed(image, Instant::now());
        reload(&mut viewer, image);
        assert!(!viewer.reloads.contains(image));
    }

    #[test]
    fn the_note_field_shows_what_is_typed_and_hides_when_done() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(200, 100, Rgba::BLUE), Format::Png).unwrap();
        let window = Window::create(PixelRect::new(0, 0, 200, 100), "note").unwrap();
        let (_events, received) = mpsc::channel();
        let mut viewer = Viewer::new(
            window.hwnd(),
            app(&window, false),
            graphics(&window),
            Worker::spawn(|| {}),
            None,
            Timings::start(),
            received,
        );
        viewer.visibility = Visibility::Shown;
        viewer.handle(Input::Request {
            id: COMMAND_LINE,
            request: open_request(&[path], false),
        });
        let output = viewer.worker.recv().unwrap();
        viewer.accept(output);
        // Focus passes to the field only while it is shown.
        viewer.focus();
        assert!(viewer.note.is_none());

        let pointer = |button, x, y| {
            [
                Input::PointerDown {
                    button,
                    at: Point::new(x, y),
                    modifiers: Modifiers::NONE,
                },
                Input::PointerMove {
                    at: Point::new(x + 40.0, y + 20.0),
                    modifiers: Modifiers::NONE,
                },
                Input::PointerUp {
                    button,
                    at: Point::new(x + 40.0, y + 20.0),
                    modifiers: Modifiers::NONE,
                },
            ]
        };
        for input in pointer(Button::Right, 10.0, 10.0) {
            viewer.input(input);
        }
        let note = viewer.note.as_ref().expect("the field is made");
        assert!(note.is_shown());
        assert_eq!(note.text(), "");
        // Text that arrives as input, as synthetic input does, is echoed in
        // the field.
        viewer.input(Input::NoteText {
            text: "typed".into(),
        });
        assert_eq!(viewer.note.as_ref().unwrap().text(), "typed");
        viewer.focus();
        viewer.input(key(Key::Enter));
        assert!(!viewer.note.as_ref().unwrap().is_shown());
        assert_eq!(viewer.app.marks().iter().next().unwrap().note, "typed");
    }

    #[test]
    fn a_shown_viewer_follows_the_colour_setting_and_a_headless_one_does_not() {
        let window = Window::create(PixelRect::new(0, 0, 100, 100), "theme").unwrap();
        let (_events, received) = mpsc::channel();
        let mut light = app(&window, false);
        light.handle(Input::Theme { dark: false }, Instant::now());
        let mut viewer = Viewer::new(
            window.hwnd(),
            light,
            graphics(&window),
            Worker::spawn(|| {}),
            None,
            Timings::start(),
            received,
        );
        let light = viewer.app.scene().background;
        viewer.theme_changed();
        assert_eq!(viewer.app.scene().background, light);

        viewer.visibility = Visibility::Hidden {
            activate: false,
            waited: false,
        };
        viewer.theme_changed();
        let mut expected = app(&window, false);
        expected.handle(
            Input::Theme {
                dark: theme::apps_dark(),
            },
            Instant::now(),
        );
        assert_eq!(viewer.app.scene().background, expected.scene().background);
    }
}
