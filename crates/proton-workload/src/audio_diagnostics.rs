//! Bounded, read-only PCM observations inside the disposable guest.
use anyhow::Result;
use std::{
    fs,
    io::Write,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub(crate) struct AudioSnapshots {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl AudioSnapshots {
    pub(crate) fn start() -> Result<Self> {
        let mut output = fs::File::create("/frames/audio-pcm.jsonl")?;
        let (stop, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            let start = Instant::now();
            for _ in 0..150 {
                let mut pcms = Vec::new();
                for card in fs::read_dir("/proc/asound")? {
                    let card = card?;
                    if !card.file_type()?.is_dir()
                        || !card.file_name().to_string_lossy().starts_with("card")
                    {
                        continue;
                    }
                    for pcm in fs::read_dir(card.path())? {
                        let pcm = pcm?;
                        let name = pcm.file_name();
                        let name = name.to_string_lossy();
                        if !name.starts_with("pcm") || !name.ends_with('p') {
                            continue;
                        }
                        for sub in fs::read_dir(pcm.path())? {
                            let sub = sub?;
                            if !sub.file_name().to_string_lossy().starts_with("sub") {
                                continue;
                            }
                            pcms.push(serde_json::json!({
                                "path": sub.path().to_string_lossy(),
                                "status": fs::read_to_string(sub.path().join("status")).ok(),
                                "hw_params": fs::read_to_string(sub.path().join("hw_params")).ok(),
                                "sw_params": fs::read_to_string(sub.path().join("sw_params")).ok(),
                            }));
                        }
                    }
                }
                output.write_all(&serde_json::to_vec(
                    &serde_json::json!({"elapsed_ms":start.elapsed().as_millis(),"playback":pcms}),
                )?)?;
                writeln!(output)?;
                output.flush()?;
                if receive.recv_timeout(Duration::from_secs(2))
                    != Err(mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            }
            Ok(())
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for AudioSnapshots {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("audio snapshots failed: {error:#}"),
                Err(_) => eprintln!("audio snapshots worker panicked"),
            }
        }
    }
}
