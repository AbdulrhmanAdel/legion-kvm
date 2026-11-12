use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use legion_display_channel::{Channel, FrameKind, Input};
use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod},
    mouse::MouseState,
    pixels::PixelFormatEnum,
};
use serde_json::json;

fn main() {
    if let Err(error) = run() {
        eprintln!("legion-presenter: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.len() == 6,
        "expected <channel> <report> <seconds> <self-test bool> <color|application> <uncapped bool>"
    );
    let report_path = PathBuf::from(&args[1]);
    let seconds: u64 = args[2].parse()?;
    ensure!((1..=60).contains(&seconds), "duration must be 1..=60");
    let self_test: bool = args[3].parse()?;
    ensure!(
        matches!(args[4].as_str(), "color" | "application"),
        "invalid presenter mode"
    );
    let application = args[4] == "application";
    let uncapped: bool = args[5].parse()?;
    let file = OpenOptions::new().read(true).write(true).open(&args[0])?;
    // SAFETY: Legion passes its private channel path, keeps the size-sealed memfd alive, and owns the sole cooperating producer in this VM.
    let channel = unsafe { Channel::attach_host(file)? };
    let mut input = channel.consumer()?;
    let (width, height) = channel.dimensions();
    let sdl = sdl2::init().map_err(anyhow::Error::msg)?;
    let video = sdl.video().map_err(anyhow::Error::msg)?;
    let window = video
        .window("Legion v2 — live Vulkan readback probe", width, height)
        .position_centered()
        .resizable()
        .build()?;
    let window_id = window.id();
    let mut builder = window.into_canvas().accelerated();
    if !uncapped {
        builder = builder.present_vsync();
    }
    let mut canvas = builder.build()?;
    if uncapped {
        // SAFETY: canvas owns this live SDL renderer on its creating thread.
        let result = unsafe { sdl2::sys::SDL_RenderSetVSync(canvas.raw(), 0) };
        ensure!(
            result == 0,
            "disable presenter vsync: {}",
            sdl2::get_error()
        );
    }
    let renderer = canvas.info().name;
    let creator = canvas.texture_creator();
    let mut texture = creator.create_texture_streaming(PixelFormatEnum::RGBA32, width, height)?;
    let mut events = sdl.event_pump().map_err(anyhow::Error::msg)?;
    let event_api = sdl.event().map_err(anyhow::Error::msg)?;
    let start = Instant::now();
    let mut first_frame = None;
    let mut frames = 0_u64;
    let mut last_number = 0;
    let mut verified = 0_u64;
    let mut generation = 0_u32;
    let mut x = 16384_u16;
    let mut y = 16384_u16;
    let mut space = false;
    let mut sent_test = false;
    let mut input_verified = false;
    let mut upload_ns = Vec::new();
    let mut gpu_ns = Vec::new();
    let mut copy_ns = Vec::new();
    let mut last_verified_generation = None;
    let mut mouse_events = 0_u64;
    let mut key_events = 0_u64;
    let mut last_input = None;
    let mut last_frame_at = Instant::now();
    let mut test_started = None;
    let mut resume_sent = false;
    let mut pause_observed = false;
    let mut last_hash = None;
    let mut changed_frames = 0_u64;
    let mut stable_frames = 0_u64;
    'display: loop {
        let mut changed = false;
        for event in events.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    ..
                } => break 'display,
                Event::MouseMotion {
                    window_id: id,
                    x: mx,
                    y: my,
                    ..
                } if id == window_id => {
                    mouse_events += 1;
                    if mouse_events <= 8 {
                        eprintln!("mouse event {mouse_events}: {mx},{my}");
                    }
                    let (w, h) = canvas.window().size();
                    x = normalized(mx, w);
                    y = normalized(my, h);
                    changed = true;
                }
                Event::KeyDown {
                    window_id: id,
                    keycode: Some(Keycode::Space),
                    repeat: false,
                    ..
                } if id == window_id => {
                    key_events += 1;
                    eprintln!("space event {key_events}");
                    space = !space;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            generation = generation
                .checked_add(1)
                .context("input generation overflow")?;
            input.set_input(Input::new(x, y, space, generation)?);
        }
        if let Some(frame) = input.read() {
            ensure!(
                frame.meta.number > last_number,
                "display frame order regressed"
            );
            last_number = frame.meta.number;
            last_frame_at = Instant::now();
            let expected = frame.meta.input.rgba(frame.meta.number);
            last_input = Some(
                json!({"x":frame.meta.input.x(),"y":frame.meta.input.y(),"space":frame.meta.input.space(),"generation":frame.meta.input.generation()}),
            );
            ensure!(
                frame.meta.kind
                    == if application {
                        FrameKind::Application
                    } else {
                        FrameKind::InputColor
                    },
                "unexpected frame kind"
            );
            if application && (!uncapped || frames < 3 || frames.is_multiple_of(120)) {
                let pixels = frame.pixels();
                let first = pixels.get(..4).context("empty application frame")?;
                ensure!(
                    pixels.as_chunks::<4>().0.iter().any(|pixel| pixel != first),
                    "application frame is uniform"
                );
                let mut hasher = DefaultHasher::new();
                pixels.hash(&mut hasher);
                let hash = hasher.finish();
                if last_hash.is_some_and(|last| last != hash) {
                    changed_frames += 1;
                    stable_frames = 0;
                } else {
                    stable_frames += 1;
                }
                last_hash = Some(hash);
                verified += 1;
                if resume_sent
                    && pause_observed
                    && !frame.meta.input.space()
                    && frame.meta.input.generation() >= 2
                {
                    input_verified = true;
                }
            } else if !application {
                ensure!(frame.meta.rgba == expected, "invalid frame color metadata");
            }
            if !application
                && (self_test || last_verified_generation != Some(frame.meta.input.generation()))
            {
                ensure!(
                    frame
                        .pixels()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| *pixel == expected),
                    "GPU readback pixels differ from input"
                );
                verified += 1;
                last_verified_generation = Some(frame.meta.input.generation());
            }
            if !application
                && sent_test
                && frame.meta.input.generation() >= 1
                && frame.meta.input.space()
                && frame.meta.input.x() > 40000
            {
                input_verified = true;
            }
            let before = Instant::now();
            texture.update(
                None,
                frame.pixels(),
                usize::try_from(width)?
                    .checked_mul(4)
                    .context("pitch overflow")?,
            )?;
            let upload = u64::try_from(before.elapsed().as_nanos())?;
            canvas
                .copy(&texture, None, None)
                .map_err(anyhow::Error::msg)?;
            canvas.present();
            if upload_ns.len() < 16384 {
                upload_ns.push(upload);
                gpu_ns.push(frame.meta.gpu_ns);
                copy_ns.push(frame.meta.copy_ns);
            }
            frames += 1;
            first_frame.get_or_insert_with(Instant::now);
        } else {
            if uncapped {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        if self_test && !sent_test && frames >= 3 {
            let (actual_width, actual_height) = canvas.window().size();
            event_api
                .push_event(Event::MouseMotion {
                    timestamp: 0,
                    window_id,
                    which: 0,
                    mousestate: MouseState::from_sdl_state(0),
                    x: i32::try_from(u64::from(actual_width) * 3 / 4)?,
                    y: i32::try_from(actual_height / 4)?,
                    xrel: 0,
                    yrel: 0,
                })
                .map_err(anyhow::Error::msg)?;
            event_api
                .push_event(Event::KeyDown {
                    timestamp: 0,
                    window_id,
                    keycode: Some(Keycode::Space),
                    scancode: None,
                    keymod: Mod::NOMOD,
                    repeat: false,
                })
                .map_err(anyhow::Error::msg)?;
            sent_test = true;
            test_started = Some(Instant::now());
        }
        if application
            && sent_test
            && !resume_sent
            && test_started.is_some_and(|t| t.elapsed() >= Duration::from_secs(1))
        {
            pause_observed =
                last_frame_at.elapsed() > Duration::from_millis(250) || stable_frames >= 5;
            event_api
                .push_event(Event::KeyDown {
                    timestamp: 0,
                    window_id,
                    keycode: Some(Keycode::Space),
                    scancode: None,
                    keymod: Mod::NOMOD,
                    repeat: false,
                })
                .map_err(anyhow::Error::msg)?;
            resume_sent = true;
        }
        if first_frame.is_some_and(|time: Instant| time.elapsed() >= Duration::from_secs(seconds)) {
            break;
        }
        ensure!(
            start.elapsed() < Duration::from_secs(seconds + 25),
            "no guest display frames before deadline"
        );
    }
    channel.stop();
    let elapsed = first_frame.map_or(0.0, |t| t.elapsed().as_secs_f64());
    let report = json!({"schema":1,"passed":frames>0 && verified>0 && (!self_test || input_verified) && (!application || changed_frames>0),"window_created":true,"uncapped":uncapped,"vsync_requested":!uncapped,"validation_every_n_frames":if uncapped {120} else {1},"application_mode":application,"changed_frames":changed_frames,"pause_observed":pause_observed,"resume_sent":resume_sent,"frames_presented":frames,"pixel_checks":verified,"input_roundtrip_verified":input_verified,"synthetic_input_test":self_test,"mouse_events":mouse_events,"space_events":key_events,"last_input":last_input,"video_driver":video.current_video_driver(),"renderer":renderer,"width":width,"height":height,"elapsed_seconds":elapsed,"delivered_fps": if elapsed>0.0 {frames.saturating_sub(1) as f64/elapsed} else {0.0},"gpu_render_readback_ms":stats(&mut gpu_ns),"shared_copy_ms":stats(&mut copy_ns),"texture_upload_ms":stats(&mut upload_ns),"transport":"ivshmem_rgba_readback","zero_copy":false});
    serde_json::to_writer_pretty(File::create(report_path)?, &report)?;
    ensure!(
        report["passed"] == true,
        "presentation or input check failed"
    );
    Ok(())
}

fn normalized(value: i32, size: u32) -> u16 {
    let value = i64::from(value).clamp(0, i64::from(size.saturating_sub(1)));
    u16::try_from(value * 65535 / i64::from(size.max(1))).unwrap_or(0)
}
fn stats(values: &mut [u64]) -> serde_json::Value {
    if values.is_empty() {
        return json!(null);
    }
    values.sort_unstable();
    let n = values.len();
    json!({"samples":n,"mean":values.iter().map(|v| *v as f64 / 1e6).sum::<f64>()/n as f64,"p50":values[n/2] as f64/1e6,"p95":values[(n-1)*95/100] as f64/1e6,"max":values[n-1] as f64/1e6})
}
