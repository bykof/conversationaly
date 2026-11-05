use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::audio::devices::configuration::{device_name, AudioDevice, DeviceType};

static SESSION_KEEPER_STARTED: AtomicBool = AtomicBool::new(false);

/// Ensure that Conversationaly's WASAPI audio sessions on all active endpoints
/// are unmuted, opted out of Windows communications auto-ducking, and not stuck
/// at the notorious 1% (0.01) Windows 11 Volume Mixer clamp.
///
/// When `force_full` is true (used at application startup), any session volume
/// below 100% (< 0.99) is restored to 1.0. When `force_full` is false (used on
/// periodic checks and stream starts), only sessions clamped at <= 5% (<= 0.05,
/// e.g. the 1% Volume Mixer bug) are reset to 1.0 so user-chosen mixer levels
/// (like 50% or 80%) are respected.
pub fn ensure_windows_audio_session_volume(force_full: bool) {
    use windows::Win32::Media::Audio::{
        eAll, eCapture, eCommunications, eConsole, eMultimedia, eRender, IAudioSessionControl2,
        IAudioSessionManager2, IChannelAudioVolume, IMMDevice, IMMDeviceEnumerator,
        ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    use windows_core::Interface;

    unsafe {
        // Initialize COM on this thread if not already initialized.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return;
        };

        let mut endpoints: Vec<IMMDevice> = Vec::new();

        // Collect default render and capture endpoints across all roles.
        for flow in [eRender, eCapture] {
            for role in [eConsole, eMultimedia, eCommunications] {
                if let Ok(dev) = enumerator.GetDefaultAudioEndpoint(flow, role) {
                    endpoints.push(dev);
                }
            }
        }

        // Also include all active endpoints so non-default selected devices are covered.
        if let Ok(collection) = enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE) {
            if let Ok(count) = collection.GetCount() {
                for idx in 0..count {
                    if let Ok(dev) = collection.Item(idx) {
                        endpoints.push(dev);
                    }
                }
            }
        }

        let current_pid = std::process::id();

        for device in endpoints {
            let Ok(manager2) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };

            let Ok(session_enum) = manager2.GetSessionEnumerator() else {
                continue;
            };

            let Ok(session_count) = session_enum.GetCount() else {
                continue;
            };

            for i in 0..session_count {
                let Ok(control) = session_enum.GetSession(i) else {
                    continue;
                };

                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };

                let pid = control2.GetProcessId().unwrap_or(0);

                // Read session identifier string to catch persisted or WebView2 sessions
                // belonging to conversationaly.exe even if the PID differs.
                let mut is_named_conversationaly = false;
                if let Ok(pwstr) = control2.GetSessionIdentifier() {
                    if !pwstr.0.is_null() {
                        let mut len = 0usize;
                        while *pwstr.0.add(len) != 0 {
                            len += 1;
                        }
                        let slice = std::slice::from_raw_parts(pwstr.0, len);
                        let session_id = String::from_utf16_lossy(slice).to_lowercase();
                        CoTaskMemFree(Some(pwstr.0 as *const std::ffi::c_void));
                        if session_id.contains("conversationaly") {
                            is_named_conversationaly = true;
                        }
                    }
                }

                if pid != current_pid && !is_named_conversationaly {
                    continue;
                }

                // Opt out of Windows communications auto-ducking so starting mic/loopback
                // never attenuates system audio or clamps the session.
                let _ = control2.SetDuckingPreference(true);

                // Restore master session volume and mute state via ISimpleAudioVolume.
                if let Ok(simple_vol) = control.cast::<ISimpleAudioVolume>() {
                    if let Ok(muted) = simple_vol.GetMute() {
                        if muted.as_bool() {
                            info!("Unmuting Windows WASAPI audio session for Conversationaly (PID {})", pid);
                            let _ = simple_vol.SetMute(false, std::ptr::null());
                        }
                    }

                    if let Ok(current_vol) = simple_vol.GetMasterVolume() {
                        let should_restore = if force_full {
                            current_vol < 0.99
                        } else {
                            current_vol <= 0.05
                        };

                        if should_restore {
                            info!(
                                "Restoring Windows Volume Mixer session volume from {:.0}% to 100% (PID {})",
                                current_vol * 100.0,
                                pid
                            );
                            let _ = simple_vol.SetMasterVolume(1.0, std::ptr::null());
                        }
                    }
                }

                // Also restore per-channel session volumes via IChannelAudioVolume in case
                // Windows PolicyConfig clamped individual channels to 1%.
                if let Ok(channel_vol) = control.cast::<IChannelAudioVolume>() {
                    if let Ok(ch_count) = channel_vol.GetChannelCount() {
                        for ch in 0..ch_count {
                            if let Ok(v) = channel_vol.GetChannelVolume(ch) {
                                let should_restore = if force_full {
                                    v < 0.99
                                } else {
                                    v <= 0.05
                                };
                                if should_restore {
                                    let _ = channel_vol.SetChannelVolume(ch, 1.0, std::ptr::null());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Build and start a silent WASAPI output render stream (`IAudioRenderClient`) on `device`.
///
/// Why this is essential on Windows 11:
/// 1. When an app only opens WASAPI capture streams (`eCapture` microphone +
///    `AUDCLNT_STREAMFLAGS_LOOPBACK` speaker capture) without an active
///    `IAudioRenderClient` output stream, Windows 11 `System > Sound > Volume mixer`
///    disables (greys out) the app's volume slider and locks it at `1` (1%).
///    Keeping a silent `IAudioRenderClient` stream active makes the session a real
///    render session so the Volume Mixer slider stays enabled at 100%.
/// 2. WASAPI loopback capture (`AUDCLNT_STREAMFLAGS_LOOPBACK`) only receives buffer
///    callbacks while the output endpoint has an active render stream. Pumping digital
///    silence (`0.0`) guarantees a continuous clock even when no other app plays audio.
pub fn build_silent_output_stream(
    device: &cpal::Device,
    preferred_config: Option<&cpal::SupportedStreamConfig>,
) -> Result<cpal::Stream> {
    let config = match preferred_config {
        Some(cfg) => cfg.clone(),
        None => device
            .default_output_config()
            .map_err(|e| anyhow!("Failed to get default output config for silent stream: {}", e))?,
    };

    let stream_config: cpal::StreamConfig = config.clone().into();
    let sample_format = config.sample_format();

    fn build_silent<T>(device: &cpal::Device, config: cpal::StreamConfig) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample,
    {
        let stream = device.build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                data.fill(T::EQUILIBRIUM);
            },
            move |err| {
                debug!("Silent WASAPI output stream notice: {}", err);
            },
            None,
        )?;
        stream.play()?;
        Ok(stream)
    }

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_silent::<f32>(device, stream_config)?,
        cpal::SampleFormat::I16 => build_silent::<i16>(device, stream_config)?,
        cpal::SampleFormat::I32 => build_silent::<i32>(device, stream_config)?,
        cpal::SampleFormat::I24 => build_silent::<cpal::I24>(device, stream_config)?,
        cpal::SampleFormat::U16 => build_silent::<u16>(device, stream_config)?,
        cpal::SampleFormat::U8 => build_silent::<u8>(device, stream_config)?,
        cpal::SampleFormat::I8 => build_silent::<i8>(device, stream_config)?,
        cpal::SampleFormat::U32 => build_silent::<u32>(device, stream_config)?,
        cpal::SampleFormat::U24 => build_silent::<cpal::U24>(device, stream_config)?,
        cpal::SampleFormat::F64 => build_silent::<f64>(device, stream_config)?,
        other => {
            return Err(anyhow!(
                "Unsupported sample format for silent output stream: {:?}",
                other
            ))
        }
    };

    Ok(stream)
}

/// Spawn a background keeper thread on Windows that maintains a silent output
/// render session on the default speaker endpoint and ensures the Windows 11
/// Volume Mixer slider for Conversationaly is enabled at 100% (never stuck at 1%).
pub fn init_windows_audio_session_keeper() {
    if SESSION_KEEPER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    std::thread::Builder::new()
        .name("windows-audio-session-keeper".to_string())
        .spawn(|| {
            info!("Starting Windows WASAPI audio session keeper");
            let mut active_device_name = String::new();
            let mut silent_stream: Option<cpal::Stream> = None;
            let mut first_run = true;

            loop {
                let host = cpal::host_from_id(cpal::HostId::Wasapi)
                    .unwrap_or_else(|_| cpal::default_host());

                if let Some(default_out) = host.default_output_device() {
                    let current_name =
                        device_name(&default_out).unwrap_or_else(|_| "Default Output".to_string());

                    if silent_stream.is_none() || current_name != active_device_name {
                        match build_silent_output_stream(&default_out, None) {
                            Ok(stream) => {
                                debug!(
                                    "Windows audio session keeper attached silent render stream to '{}'",
                                    current_name
                                );
                                active_device_name = current_name;
                                silent_stream = Some(stream);
                            }
                            Err(e) => {
                                debug!(
                                    "Windows audio session keeper could not open silent stream on '{}': {}",
                                    current_name, e
                                );
                            }
                        }
                    }
                }

                ensure_windows_audio_session_volume(first_run);
                first_run = false;

                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        })
        .ok();
}

/// Configure Windows audio devices using WASAPI
pub fn configure_windows_audio(host: &cpal::Host) -> Result<Vec<AudioDevice>> {
    // Ensure the Windows audio session keeper is active so Volume Mixer stays at 100%
    init_windows_audio_session_keeper();
    ensure_windows_audio_session_volume(false);

    let mut devices = Vec::new();

    // Get WASAPI devices
    if let Ok(wasapi_host) = cpal::host_from_id(cpal::HostId::Wasapi) {
        debug!("Using WASAPI host for Windows audio device enumeration");

        // Add output devices (including loopback)
        if let Ok(output_devices) = wasapi_host.output_devices() {
            for device in output_devices {
                if let Ok(name) = device_name(&device) {
                    // For Windows, we need to mark output devices specifically for loopback
                    devices.push(AudioDevice::new(name.clone(), DeviceType::Output));
                }
            }
        } else {
            warn!("Failed to enumerate WASAPI output devices");
        }

        // Add input devices from WASAPI
        if let Ok(input_devices) = wasapi_host.input_devices() {
            for device in input_devices {
                if let Ok(name) = device_name(&device) {
                    devices.push(AudioDevice::new(name.clone(), DeviceType::Input));
                }
            }
        } else {
            warn!("Failed to enumerate WASAPI input devices");
        }
    } else {
        warn!("Failed to create WASAPI host, falling back to default host");
    }

    // If WASAPI failed or returned no devices, try default host as fallback
    if devices.is_empty() {
        debug!("WASAPI device enumeration failed or returned no devices, falling back to default host");
        // Add regular input devices
        if let Ok(input_devices) = host.input_devices() {
            for device in input_devices {
                if let Ok(name) = device_name(&device) {
                    devices.push(AudioDevice::new(name.clone(), DeviceType::Input));
                }
            }
        } else {
            warn!("Failed to enumerate input devices from default host");
        }

        // Add output devices
        if let Ok(output_devices) = host.output_devices() {
            for device in output_devices {
                if let Ok(name) = device_name(&device) {
                    devices.push(AudioDevice::new(name.clone(), DeviceType::Output));
                }
            }
        } else {
            warn!("Failed to enumerate output devices from default host");
        }
    }

    // If we still have no devices, add default devices
    if devices.is_empty() {
        warn!("No audio devices found, adding default devices only");

        // Try to add default input device
        if let Some(device) = host.default_input_device() {
            if let Ok(name) = device_name(&device) {
                devices.push(AudioDevice::new(name, DeviceType::Input));
            }
        }

        // Try to add default output device
        if let Some(device) = host.default_output_device() {
            if let Ok(name) = device_name(&device) {
                devices.push(AudioDevice::new(name, DeviceType::Output));
            }
        }
    }

    debug!("Found {} Windows audio devices", devices.len());
    Ok(devices)
}

/// Helper to pick the best sample rate from a supported config range.
/// Prioritizes standard 48,000 Hz (conversationaly's target rate) if supported,
/// otherwise falls back to min_sample_rate to avoid high sampling rates (96k/192k)
/// that cause buffer starvation and downsampling artifacts.
fn pick_best_sample_rate(config: &cpal::SupportedStreamConfigRange) -> cpal::SupportedStreamConfig {
    const TARGET_RATE: cpal::SampleRate = 48000;
    let sample_rate = if config.min_sample_rate() <= TARGET_RATE && TARGET_RATE <= config.max_sample_rate() {
        TARGET_RATE
    } else {
        config.min_sample_rate()
    };
    config.with_sample_rate(sample_rate)
}

/// Resolve input config for a matched WASAPI input device
fn resolve_input_device_config(
    device: cpal::Device,
    name: &str,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    match device.default_input_config() {
        Ok(default_config) => Ok((device, default_config)),
        Err(e) => {
            warn!(
                "Failed to get default input config for '{}': {}. Trying supported configs...",
                name, e
            );

            if let Ok(supported_configs) = device.supported_input_configs() {
                let configs: Vec<_> = supported_configs.collect();
                if !configs.is_empty() {
                    for config in &configs {
                        if config.sample_format() == cpal::SampleFormat::F32 && config.channels() == 2 {
                            return Ok((device, pick_best_sample_rate(config)));
                        }
                    }

                    for config in &configs {
                        if config.sample_format() == cpal::SampleFormat::F32 {
                            return Ok((device, pick_best_sample_rate(config)));
                        }
                    }

                    let config = pick_best_sample_rate(&configs[0]);
                    info!("Using fallback input config: {:?}", config);
                    return Ok((device, config));
                }
            }

            Err(anyhow!("No compatible input configuration found for device: {}", name))
        }
    }
}

/// Resolve output config for a matched WASAPI output (loopback) device
fn resolve_output_device_config(
    device: cpal::Device,
    name: &str,
) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    // CRITICAL FOR WASAPI LOOPBACK:
    // In WASAPI shared mode loopback capture (AUDCLNT_STREAMFLAGS_LOOPBACK),
    // the capture stream MUST match the Windows Audio Engine mix format
    // returned by IAudioClient::GetMixFormat() (i.e. default_output_config()).
    match device.default_output_config() {
        Ok(default_config) => {
            info!(
                "Using default mix format output config for loopback on '{}': {:?}",
                name, default_config
            );
            Ok((device, default_config))
        }
        Err(e) => {
            warn!(
                "Failed to get default output config for '{}': {}. Trying supported configs...",
                name, e
            );

            if let Ok(supported_configs) = device.supported_output_configs() {
                let configs: Vec<_> = supported_configs.collect();
                if !configs.is_empty() {
                    for config in &configs {
                        if config.sample_format() == cpal::SampleFormat::F32 && config.channels() == 2 {
                            let config = pick_best_sample_rate(config);
                            info!("Using stereo F32 output config: {:?}", config);
                            return Ok((device, config));
                        }
                    }

                    for config in &configs {
                        if config.sample_format() == cpal::SampleFormat::F32 {
                            let config = pick_best_sample_rate(config);
                            info!("Using F32 output config: {:?}", config);
                            return Ok((device, config));
                        }
                    }

                    let config = pick_best_sample_rate(&configs[0]);
                    info!("Using fallback output config: {:?}", config);
                    return Ok((device, config));
                }
            }

            Err(anyhow!("No compatible output configuration found for device: {}", name))
        }
    }
}

/// Get Windows device and configuration using WASAPI
pub fn get_windows_device(audio_device: &AudioDevice) -> Result<(cpal::Device, cpal::SupportedStreamConfig)> {
    init_windows_audio_session_keeper();
    ensure_windows_audio_session_volume(false);

    let wasapi_host = cpal::host_from_id(cpal::HostId::Wasapi)
        .map_err(|e| anyhow!("Failed to create WASAPI host: {}", e))?;

    // Extract the base device name without the (input) or (output) suffix
    let base_name = if audio_device.name.ends_with(" (input)") {
        audio_device.name.trim_end_matches(" (input)")
    } else if audio_device.name.ends_with(" (output)") {
        audio_device.name.trim_end_matches(" (output)")
    } else {
        &audio_device.name
    };

    info!("Looking for Windows device with base name: {}", base_name);

    match audio_device.device_type {
        DeviceType::Input => {
            let mut partial_match: Option<(cpal::Device, String)> = None;
            for device in wasapi_host.input_devices()? {
                if let Ok(name) = device_name(&device) {
                    info!("Checking input device: {}", name);
                    if name == base_name {
                        return resolve_input_device_config(device, &name);
                    } else if partial_match.is_none() && name.contains(base_name) {
                        partial_match = Some((device, name));
                    }
                }
            }

            if let Some((device, name)) = partial_match {
                return resolve_input_device_config(device, &name);
            }

            // If we didn't find a matching device, try the default input device as fallback
            info!("No matching input device found, trying default input device");
            if let Some(default_device) = wasapi_host.default_input_device() {
                if let Ok(name) = device_name(&default_device) {
                    return resolve_input_device_config(default_device, &name);
                }
            }
        }
        DeviceType::Output => {
            let mut partial_match: Option<(cpal::Device, String)> = None;
            for device in wasapi_host.output_devices()? {
                if let Ok(name) = device_name(&device) {
                    info!("Checking output device: {}", name);
                    if name == base_name {
                        return resolve_output_device_config(device, &name);
                    } else if partial_match.is_none() && name.contains(base_name) {
                        partial_match = Some((device, name));
                    }
                }
            }

            if let Some((device, name)) = partial_match {
                return resolve_output_device_config(device, &name);
            }

            // If we didn't find a matching device, try the default output device as fallback
            info!("No matching output device found, trying default output device");
            if let Some(default_device) = wasapi_host.default_output_device() {
                if let Ok(name) = device_name(&default_device) {
                    info!("Using default output device: {}", name);
                    return resolve_output_device_config(default_device, &name);
                }
            }
        }
    }

    Err(anyhow!("Device not found or no compatible configuration available: {}", audio_device.name))
}
