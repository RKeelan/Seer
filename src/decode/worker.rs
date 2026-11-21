//! The decode thread and its queue.
//!
//! A [`Worker`] decodes [`Job`]s in the order they are submitted, on a thread
//! of its own, and hands back an [`Output`] for each. After each output it
//! calls the `notify` function it was spawned with, which the window uses to
//! post itself a message. It keeps the last file it opened, so a scaled decode
//! followed by a full one reads the file's header once.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

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

/// A thread that decodes images.
pub struct Worker {
    jobs: Option<Sender<Job>>,
    outputs: Receiver<Output>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread. It calls `notify` after each output is ready.
    pub fn spawn(notify: impl Fn() + Send + 'static) -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (send, outputs) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || run(&queue, &send, &notify))
            .expect("the decode thread starts");
        Worker {
            jobs: Some(jobs),
            outputs,
            thread: Some(thread),
        }
    }

    /// Queues a job.
    pub fn submit(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            // The thread outlives the sender, so this cannot fail.
            let _ = jobs.send(job);
        }
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
        self.jobs = None;
        while self.outputs.try_recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(queue: &Receiver<Job>, outputs: &Sender<Output>, notify: &dyn Fn()) {
    let decoder = Decoder::new();
    let mut open: Option<Source> = None;
    while let Ok(job) = queue.recv() {
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
    if open.as_ref().is_none_or(|source| source.path() != job.path) {
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
}
