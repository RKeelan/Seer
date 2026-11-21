//! The decode thread and its queue.
//!
//! A [`Worker`] decodes [`Job`]s on a thread of its own and hands back an
//! [`Output`] for each. After each output it calls the `notify` function it
//! was spawned with, which the window uses to post itself a message. While
//! jobs are queued it keeps the last file it opened, so a scaled decode
//! followed by a full one reads the file's header once, and opens it again if
//! the file has changed since. It closes the file whenever the queue runs dry,
//! so the viewer never holds a shown image's file open.
//!
//! Jobs come at two priorities. Those [submitted](Worker::submit) are for
//! images on screen, and run in the order they came. Those
//! [prefetched](Worker::prefetch) are decodes ahead of need, and run, also in
//! order, only when no submitted job is waiting. [`Worker::cancel`] drops an
//! image's queued jobs of either kind; a job already running finishes.
//!
//! The viewer decodes thumbnails on a second worker made with
//! [`Worker::spawn_background`], whose thread Windows runs only when nothing
//! at normal priority wants the processor or the disk. A thumbnail decode
//! then never holds up an image to be shown, even one that takes long, such
//! as a large PNG, which has to be decoded in full to be scaled down.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
};

use crate::decode::wic::{Decoded, Decoder, Source};
use crate::error::Result;
use crate::model::ImageId;
use crate::model::app::DecodeTarget;

/// Something to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The image the result belongs to.
    pub image: ImageId,
    /// The file.
    pub path: PathBuf,
    /// What to decode.
    pub target: DecodeTarget,
}

/// What became of a [`Job`].
#[derive(Debug)]
pub struct Output {
    /// The image.
    pub image: ImageId,
    /// What was asked for.
    pub target: DecodeTarget,
    /// The decoded image, or why it could not be decoded.
    pub result: Result<Decoded>,
    /// When the worker started on the job.
    pub started: Instant,
    /// When it finished.
    pub finished: Instant,
}

/// The jobs waiting for the thread.
#[derive(Debug, Default)]
struct Queue {
    urgent: VecDeque<Job>,
    background: VecDeque<Job>,
    closed: bool,
}

impl Queue {
    /// The next job to run: the oldest urgent one, else the oldest in the
    /// background.
    fn next(&mut self) -> Option<Job> {
        self.urgent
            .pop_front()
            .or_else(|| self.background.pop_front())
    }

    /// Drops the queued jobs for `image`.
    fn cancel(&mut self, image: ImageId) {
        self.urgent.retain(|job| job.image != image);
        self.background.retain(|job| job.image != image);
    }

    /// Drops every queued job, and makes the thread end once it has finished
    /// the job in hand.
    fn close(&mut self) {
        self.closed = true;
        self.urgent.clear();
        self.background.clear();
    }
}

/// The queue and the signal that it has changed, shared with the thread.
#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // Nothing panics while holding the lock; were it poisoned, the queue
        // would still be consistent.
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the queue and wakes the thread.
    fn change(&self, change: impl FnOnce(&mut Queue)) {
        change(&mut self.lock());
        self.changed.notify_one();
    }

    /// Waits for the next job, or for the queue to close. If the queue is
    /// empty, calls `idle` without holding the lock before waiting.
    fn wait(&self, idle: impl FnOnce()) -> Option<Job> {
        let mut idle = Some(idle);
        let mut queue = self.lock();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(job) = queue.next() {
                return Some(job);
            }
            if let Some(idle) = idle.take() {
                drop(queue);
                idle();
                queue = self.lock();
                continue;
            }
            queue = self
                .changed
                .wait(queue)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// A thread that decodes images.
pub struct Worker {
    shared: Arc<Shared>,
    outputs: Receiver<Output>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread. It calls `notify` after each output is ready.
    pub fn spawn(notify: impl Fn() + Send + 'static) -> Self {
        Worker::start(false, notify)
    }

    /// Starts the thread at background priority, which lowers its priority
    /// for the processor, the disk and memory. It calls `notify` after each
    /// output is ready.
    pub fn spawn_background(notify: impl Fn() + Send + 'static) -> Self {
        Worker::start(true, notify)
    }

    fn start(background: bool, notify: impl Fn() + Send + 'static) -> Self {
        let shared = Arc::new(Shared::default());
        let (send, outputs) = mpsc::channel();
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(if background { "thumbnails" } else { "decode" }.into())
                .spawn(move || {
                    if background {
                        // SAFETY: the pseudo-handle names this thread. On
                        // failure the thread runs at normal priority.
                        let _ = unsafe {
                            SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN)
                        };
                    }
                    run(&shared, &send, &notify)
                })
                .expect("the decode thread starts")
        };
        Worker {
            shared,
            outputs,
            thread: Some(thread),
        }
    }

    /// Queues a job for an image on screen, behind the other such jobs and
    /// ahead of every prefetch.
    pub fn submit(&self, job: Job) {
        self.shared.change(|queue| queue.urgent.push_back(job));
    }

    /// Queues a decode ahead of need, to run when no submitted job waits.
    pub fn prefetch(&self, job: Job) {
        self.shared.change(|queue| queue.background.push_back(job));
    }

    /// Drops the queued jobs for `image`. A job already running finishes, and
    /// its output still arrives.
    pub fn cancel(&self, image: ImageId) {
        self.shared.change(|queue| queue.cancel(image));
    }

    /// Waits for the next output.
    pub fn recv(&self) -> Option<Output> {
        self.outputs.recv().ok()
    }

    /// The next output, if one is ready.
    pub fn try_recv(&self) -> Option<Output> {
        self.outputs.try_recv().ok()
    }
}

impl Drop for Worker {
    /// Lets the thread finish the job in hand, then waits for it to end.
    /// Queued jobs are dropped.
    fn drop(&mut self) {
        self.shared.change(Queue::close);
        while self.outputs.try_recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: &Shared, outputs: &Sender<Output>, notify: &dyn Fn()) {
    let decoder = Decoder::new();
    let mut open: Option<Source> = None;
    // Nothing more to read from the file for now: close it.
    while let Some(job) = shared.wait(|| open = None) {
        let started = Instant::now();
        let result = match &decoder {
            Ok(decoder) => decode(decoder, &mut open, &job),
            Err(error) => Err(error.clone()),
        };
        let output = Output {
            image: job.image,
            target: job.target,
            result,
            started,
            finished: Instant::now(),
        };
        if outputs.send(output).is_err() {
            return;
        }
        notify();
    }
}

fn decode(decoder: &Decoder, open: &mut Option<Source>, job: &Job) -> Result<Decoded> {
    if open
        .as_ref()
        .is_none_or(|source| source.path() != job.path || !source.is_current())
    {
        *open = None;
        *open = Some(decoder.open(&job.path)?);
    }
    let source = open.as_ref().expect("opened above");
    source.decode(job.target)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::decode::wic::Decoder;
    use crate::error::ErrorCode;
    use crate::model::geometry::PixelSize;
    use crate::testutil::{Format, Pixels, Rgba, TempDir, write_image};

    #[test]
    fn jobs_are_decoded_in_order_and_notified() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(40, 20, Rgba::BLUE), Format::Png).unwrap();
        let notified = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&notified);
        let worker = Worker::spawn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let scaled = DecodeTarget::Scaled {
            at_least: PixelSize::new(10, 5),
        };
        for (image, path, target) in [
            (1, path.clone(), scaled),
            (1, path.clone(), DecodeTarget::Full),
            (2, dir.join("absent.png"), DecodeTarget::Full),
        ] {
            worker.submit(Job {
                image: ImageId(image),
                path,
                target,
            });
        }
        let first = worker.recv().unwrap();
        assert_eq!(first.target, scaled);
        assert_eq!(first.result.unwrap().bitmap_size, PixelSize::new(10, 5));
        assert!(first.finished >= first.started);
        let second = worker.recv().unwrap();
        assert_eq!(second.result.unwrap().bitmap_size, PixelSize::new(40, 20));
        let third = worker.recv().unwrap();
        assert_eq!(third.image, ImageId(2));
        assert_eq!(third.result.unwrap_err().code, ErrorCode::FileNotFound);
        assert!(worker.try_recv().is_none());
        drop(worker);
        assert_eq!(notified.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_slow_thumbnail_does_not_hold_up_an_image_to_be_shown() {
        let dir = TempDir::new().unwrap();
        // Noise compresses badly, so this PNG is slow to decode, and a PNG
        // is decoded in full even for a thumbnail.
        let large = dir.join("large.png");
        let noise = Pixels::from_fn(2000, 2000, |x, y| {
            let n = x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503);
            Rgba::opaque(n as u8, (n >> 8) as u8, (n >> 16) as u8)
        });
        write_image(&large, &noise, Format::Png).unwrap();
        let small = dir.join("small.png");
        write_image(&small, &Pixels::solid(40, 20, Rgba::BLUE), Format::Png).unwrap();

        let thumbnailer = Worker::spawn_background(|| {});
        let worker = Worker::spawn(|| {});
        thumbnailer.submit(Job {
            image: ImageId(1),
            path: large,
            target: DecodeTarget::Thumbnail {
                at_most: PixelSize::new(64, 48),
            },
        });
        worker.submit(Job {
            image: ImageId(2),
            path: small,
            target: DecodeTarget::Full,
        });
        let shown = worker.recv().unwrap();
        let thumbnail = thumbnailer.recv().unwrap();
        assert!(shown.result.is_ok());
        assert_eq!(
            thumbnail.result.unwrap().bitmap_size,
            PixelSize::new(48, 48)
        );
        assert!(shown.finished < thumbnail.finished);
    }

    #[test]
    fn the_file_is_closed_once_the_queue_runs_dry() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(40, 20, Rgba::BLUE), Format::Png).unwrap();
        let worker = Worker::spawn(|| {});
        worker.submit(Job {
            image: ImageId(1),
            path: path.clone(),
            target: DecodeTarget::Full,
        });
        assert!(worker.recv().unwrap().result.is_ok());
        // Opening the file sharing nothing succeeds only once no other
        // handle to it is open.
        let exclusive = || {
            std::fs::OpenOptions::new()
                .write(true)
                .share_mode(0)
                .open(&path)
        };
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while exclusive().is_err() {
            assert!(Instant::now() < deadline, "the worker kept the file open");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_file_rewritten_after_decoding_is_read_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(40, 20, Rgba::BLUE), Format::Png).unwrap();
        let decoder = Decoder::new().unwrap();
        let mut open = None;
        let job = Job {
            image: ImageId(1),
            path: path.clone(),
            target: DecodeTarget::Full,
        };
        let first = decode(&decoder, &mut open, &job).unwrap();
        assert_eq!(first.size, PixelSize::new(40, 20));
        // The open file does not stop another program rewriting it.
        write_image(&path, &Pixels::solid(30, 10, Rgba::RED), Format::Png).unwrap();
        let second = decode(&decoder, &mut open, &job).unwrap();
        assert_eq!(second.size, PixelSize::new(30, 10));
        std::fs::remove_file(&path).unwrap();
    }

    fn job(image: u32, path: &std::path::Path) -> Job {
        Job {
            image: ImageId(image),
            path: path.to_owned(),
            target: DecodeTarget::Full,
        }
    }

    #[test]
    fn the_queue_runs_urgent_jobs_first_and_cancels_by_image() {
        let path = std::path::Path::new("x.png");
        let mut queue = Queue::default();
        queue.background.push_back(job(1, path));
        queue.background.push_back(job(2, path));
        queue.urgent.push_back(job(3, path));
        queue.urgent.push_back(job(2, path));
        queue.urgent.push_back(job(4, path));
        queue.cancel(ImageId(2));
        let order: Vec<u32> = std::iter::from_fn(|| queue.next())
            .map(|job| job.image.0)
            .collect();
        assert_eq!(order, [3, 4, 1]);
        queue.background.push_back(job(5, path));
        queue.close();
        assert!(queue.closed && queue.next().is_none());
    }

    #[test]
    fn prefetches_wait_for_submitted_jobs_and_cancelled_ones_never_run() {
        let dir = TempDir::new().unwrap();
        let path = dir.join("image.png");
        write_image(&path, &Pixels::solid(8, 8, Rgba::RED), Format::Png).unwrap();
        let worker = Worker::spawn(|| {});
        // Queue everything before the thread can take any of it.
        {
            let mut queue = worker.shared.lock();
            queue.background.push_back(job(1, &path));
            queue.background.push_back(job(2, &path));
            queue.urgent.push_back(job(3, &path));
            queue.cancel(ImageId(2));
        }
        worker.shared.changed.notify_one();
        assert_eq!(worker.recv().unwrap().image, ImageId(3));
        assert_eq!(worker.recv().unwrap().image, ImageId(1));
        // The public methods queue and cancel the same way.
        worker.prefetch(job(4, &path));
        assert_eq!(worker.recv().unwrap().image, ImageId(4));
        worker.cancel(ImageId(9));
        worker.submit(job(5, &path));
        assert_eq!(worker.recv().unwrap().image, ImageId(5));
        assert!(worker.try_recv().is_none());
    }
}
