//! ALSA playback and mixer initialization.
use std::ffi::{CStr, CString, c_char, c_void};
#[link(name = "libasound.so.2", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn snd_pcm_open(pcm: *mut *mut c_void, name: *const c_char, stream: i32, mode: i32) -> i32;
    fn snd_pcm_set_params(
        pcm: *mut c_void,
        format: i32,
        access: i32,
        channels: u32,
        rate: u32,
        resample: i32,
        latency: u32,
    ) -> i32;
    fn snd_pcm_writei(pcm: *mut c_void, data: *const c_void, frames: u64) -> i64;
    fn snd_pcm_drain(pcm: *mut c_void) -> i32;
    fn snd_pcm_close(pcm: *mut c_void) -> i32;
    fn snd_pcm_state(pcm: *mut c_void) -> i32;
    fn snd_mixer_open(mixer: *mut *mut c_void, mode: i32) -> i32;
    fn snd_mixer_attach(mixer: *mut c_void, name: *const c_char) -> i32;
    fn snd_mixer_selem_register(
        mixer: *mut c_void,
        options: *mut c_void,
        class: *mut *mut c_void,
    ) -> i32;
    fn snd_mixer_load(mixer: *mut c_void) -> i32;
    fn snd_mixer_close(mixer: *mut c_void) -> i32;
    fn snd_mixer_first_elem(mixer: *mut c_void) -> *mut c_void;
    fn snd_mixer_elem_next(element: *mut c_void) -> *mut c_void;
    fn snd_mixer_selem_get_name(element: *mut c_void) -> *const c_char;
    fn snd_mixer_selem_has_playback_switch(element: *mut c_void) -> i32;
    fn snd_mixer_selem_set_playback_switch_all(element: *mut c_void, value: i32) -> i32;
    fn snd_mixer_selem_has_playback_volume(element: *mut c_void) -> i32;
    fn snd_mixer_selem_set_playback_dB_all(element: *mut c_void, value: i64, direction: i32)
    -> i32;
    fn snd_mixer_selem_get_playback_volume_range(
        element: *mut c_void,
        min: *mut i64,
        max: *mut i64,
    ) -> i32;
    fn snd_mixer_selem_set_playback_volume_all(element: *mut c_void, value: i64) -> i32;
}
struct Mixer(*mut c_void);
impl Drop for Mixer {
    fn drop(&mut self) {
        // SAFETY: this object owns a successfully opened mixer.
        unsafe {
            snd_mixer_close(self.0);
        }
    }
}
fn initialize_mixer(card: u32) -> Result<(), String> {
    // This helper is staged in the disposable guest; refuse accidental host use.
    if !std::path::Path::new("/frames/audio-runtime.json").is_file() {
        return Err("guest audio runtime report is missing".into());
    }
    let check = |result: i32| {
        if result < 0 {
            Err(format!("initialize guest playback mixer: {result}"))
        } else {
            Ok(())
        }
    };
    let name = CString::new(format!("hw:{card}")).map_err(|e| e.to_string())?;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: valid output pointer; no mixer exists before this succeeds.
    check(unsafe { snd_mixer_open(&mut pointer, 0) })?;
    let mixer = Mixer(pointer);
    // SAFETY: owned mixer and terminated card name; null options request defaults.
    unsafe {
        check(snd_mixer_attach(mixer.0, name.as_ptr()))?;
        check(snd_mixer_selem_register(
            mixer.0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ))?;
        check(snd_mixer_load(mixer.0))?;
    }
    // SAFETY: loaded mixer owns all element pointers until its close.
    let mut element = unsafe { snd_mixer_first_elem(mixer.0) };
    let mut controls = 0;
    while !element.is_null() {
        // SAFETY: each element belongs to the loaded live mixer. Only playback
        // switches/gain are changed, never capture controls or host devices.
        unsafe {
            let mut changed = false;
            if snd_mixer_selem_has_playback_switch(element) != 0 {
                check(snd_mixer_selem_set_playback_switch_all(element, 1))?;
                changed = true;
            }
            if snd_mixer_selem_has_playback_volume(element) != 0 {
                if snd_mixer_selem_set_playback_dB_all(element, 0, 0) < 0 {
                    let (mut min, mut max) = (0, 0);
                    check(snd_mixer_selem_get_playback_volume_range(
                        element, &mut min, &mut max,
                    ))?;
                    check(snd_mixer_selem_set_playback_volume_all(element, max))?;
                }
                changed = true;
            }
            if changed {
                eprintln!(
                    "guest playback control initialized: {}",
                    CStr::from_ptr(snd_mixer_selem_get_name(element)).to_string_lossy()
                );
                controls += 1;
            }
            element = snd_mixer_elem_next(element);
        }
    }
    println!("{{\"schema\":1,\"playback_controls_initialized\":{controls},\"microphone\":false}}");
    Ok(())
}
struct Pcm(*mut c_void);
impl Drop for Pcm {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns a successfully opened PCM.
        unsafe {
            snd_pcm_close(self.0);
        }
    }
}
fn run() -> Result<(), String> {
    let mut pointer = std::ptr::null_mut();
    // SAFETY: valid output pointer and terminated default endpoint name.
    let result = unsafe { snd_pcm_open(&mut pointer, c"default".as_ptr(), 0, 0) };
    if result < 0 {
        return Err(format!("open default playback: {result}"));
    }
    let pcm = Pcm(pointer);
    // SAFETY: owned PCM; ALSA enums S16_LE=2, RW_INTERLEAVED=3.
    let result = unsafe { snd_pcm_set_params(pcm.0, 2, 3, 2, 48_000, 1, 40_000) };
    if result < 0 {
        return Err(format!("configure stereo PCM: {result}"));
    }
    let mut samples = Vec::with_capacity(9_600 * 2);
    for frame in 0..9_600 {
        // 240 Hz triangle, under 1% of full-scale amplitude for 200 ms.
        let phase = frame % 200;
        let triangle = if phase < 100 { phase } else { 200 - phase };
        let sample = ((triangle - 50) * 5) as i16;
        samples.extend([sample, sample]);
    }
    let mut written = 0;
    while written < 9_600 {
        // SAFETY: offset and frame count stay within the interleaved slice.
        let result = unsafe {
            snd_pcm_writei(
                pcm.0,
                samples[written * 2..].as_ptr().cast(),
                (9_600 - written) as u64,
            )
        };
        if result <= 0 || result as usize > 9_600 - written {
            // SAFETY: state is queried while the PCM remains owned and open.
            let state = unsafe { snd_pcm_state(pcm.0) };
            for name in ["status", "hw_params", "sw_params"] {
                if let Ok(value) =
                    std::fs::read_to_string(format!("/proc/asound/card0/pcm0p/sub0/{name}"))
                {
                    eprintln!("PCM {name}: {value}");
                }
            }
            return Err(format!(
                "write stereo PCM: {result}; PCM state={state}; written={written}"
            ));
        }
        written += result as usize;
    }
    // SAFETY: waits for playback queued to this owned PCM to complete.
    let result = unsafe { snd_pcm_drain(pcm.0) };
    if result < 0 {
        return Err(format!("drain playback: {result}"));
    }
    println!(
        "{{\"schema\":1,\"passed\":true,\"channels\":2,\"sample_rate\":48000,\"frames\":{written},\"microphone\":false}}"
    );
    Ok(())
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [] => run(),
        [mode, card] if mode == "--initialize-mixer" => card
            .parse::<u32>()
            .map_err(|e| e.to_string())
            .and_then(initialize_mixer),
        _ => Err("expected no arguments or --initialize-mixer CARD".into()),
    };
    if let Err(error) = result {
        eprintln!("legion audio: {error}");
        std::process::exit(1);
    }
}
