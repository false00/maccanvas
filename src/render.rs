use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    images::{Image, ImageRef},
};

use crate::{
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::Viewport,
};

pub struct Job {
    pub frame: Arc<Framebuffer>,
    pub viewport: Viewport,
    pub generation: u64,
    pub ticket: u64,
}

pub struct Rendered {
    pub viewport: Viewport,
    pub generation: u64,
    pub ticket: u64,
    pub rgba: Vec<u8>,
}

type Queue = Arc<(Mutex<Option<Job>>, Condvar)>;
type Output = Arc<Mutex<Option<Result<Rendered>>>>;

pub struct Renderer {
    queue: Queue,
    output: Output,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

fn scale(resizer: &mut Resizer, job: &Job) -> Result<Vec<u8>> {
    if job.frame.size == job.viewport.pixels {
        return Ok(job.frame.rgba.clone());
    }
    let input = ImageRef::new(
        job.frame.size.width,
        job.frame.size.height,
        &job.frame.rgba,
        PixelType::U8x4,
    )
    .map_err(|error| Error::Display(error.to_string()))?;
    let mut output = Image::new(
        job.viewport.pixels.width,
        job.viewport.pixels.height,
        PixelType::U8x4,
    );
    resizer
        .resize(
            &input,
            &mut output,
            &ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
                .use_alpha(false),
        )
        .map_err(|error| Error::Display(error.to_string()))?;
    Ok(output.into_vec())
}

impl Renderer {
    pub fn new() -> Self {
        let queue: Queue = Arc::new((Mutex::new(None), Condvar::new()));
        let output: Output = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let incoming = queue.clone();
        let outgoing = output.clone();
        let stopping = stopped.clone();
        let worker = thread::spawn(move || {
            let mut resizer = Resizer::new();
            loop {
                let (lock, ready) = &*incoming;
                let mut pending = match lock.lock() {
                    Ok(pending) => pending,
                    Err(_) => {
                        if let Ok(mut slot) = outgoing.lock() {
                            *slot = Some(Err(Error::Display(
                                "Rendering queue was interrupted".into(),
                            )));
                        }
                        return;
                    }
                };
                while pending.is_none() && !stopping.load(Ordering::Acquire) {
                    pending = match ready.wait(pending) {
                        Ok(pending) => pending,
                        Err(_) => {
                            if let Ok(mut slot) = outgoing.lock() {
                                *slot = Some(Err(Error::Display(
                                    "Rendering wait was interrupted".into(),
                                )));
                            }
                            return;
                        }
                    };
                }
                if stopping.load(Ordering::Acquire) {
                    return;
                }
                let Some(job) = pending.take() else { continue };
                drop(pending);
                let result = scale(&mut resizer, &job).map(|rgba| Rendered {
                    viewport: job.viewport,
                    generation: job.generation,
                    ticket: job.ticket,
                    rgba,
                });
                match outgoing.lock() {
                    Ok(mut slot) => *slot = Some(result),
                    Err(_) => {
                        eprintln!("MacCanvas rendering output was interrupted");
                        return;
                    }
                }
            }
        });
        Self {
            queue,
            output,
            stopped,
            worker: Some(worker),
        }
    }

    pub fn request(&self, job: Job) -> Result<()> {
        let mut pending = self
            .queue
            .0
            .lock()
            .map_err(|_| Error::Display("Rendering queue is unavailable".into()))?;
        *pending = Some(job);
        self.queue.1.notify_one();
        Ok(())
    }

    pub fn take(&self) -> Result<Option<Rendered>> {
        let mut output = self
            .output
            .lock()
            .map_err(|_| Error::Display("Rendering output is unavailable".into()))?;
        if let Some(result) = output.take() {
            return result.map(Some);
        }
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(Error::Display(
                "Rendering worker stopped unexpectedly".into(),
            ));
        }
        Ok(None)
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let pending = self.queue.0.lock();
        if pending.is_err() {
            eprintln!("MacCanvas rendering queue was interrupted during shutdown");
        }
        self.stopped.store(true, Ordering::Release);
        self.queue.1.notify_one();
        drop(pending);
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            eprintln!("MacCanvas rendering worker failed during shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Extent, Rect};

    #[test]
    fn four_k_lanczos_output_is_exactly_1440p_and_opaque() {
        let mut frame = Framebuffer::new(Extent::new(3840, 2160).unwrap());
        frame
            .fill(
                Rect {
                    x: 0,
                    y: 0,
                    width: 3840,
                    height: 2160,
                },
                [37, 80, 120, 255],
            )
            .unwrap();
        let job = Job {
            viewport: Viewport::fit(frame.size, 2560.0, 1440.0, 1.0).unwrap(),
            frame: Arc::new(frame),
            generation: 1,
            ticket: 1,
        };
        let output = scale(&mut Resizer::new(), &job).unwrap();
        assert_eq!(output.len(), 2560 * 1440 * 4);
        assert!(
            output
                .chunks_exact(4)
                .all(|pixel| pixel == [37, 80, 120, 255])
        );
    }

    #[test]
    fn latest_request_is_not_lost_while_resizing() {
        let renderer = Renderer::new();
        let frame = Arc::new(Framebuffer::new(Extent::new(64, 64).unwrap()));
        for ticket in 1..=10 {
            renderer
                .request(Job {
                    frame: frame.clone(),
                    viewport: Viewport::fit(frame.size, 32.0, 32.0, 1.0).unwrap(),
                    generation: 1,
                    ticket,
                })
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if renderer
                .take()
                .unwrap()
                .is_some_and(|result| result.ticket == 10)
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
