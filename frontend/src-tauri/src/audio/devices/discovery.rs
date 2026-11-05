use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::error;

use super::configuration::{device_name, AudioDevice, DeviceType};
use super::platform;

/// List all available audio devices on the system
pub async fn list_audio_devices() -> Result<Vec<AudioDevice>> {
    let host = cpal::default_host();

    // Platform-specific device enumeration
    #[allow(unused_mut)]
    let mut devices = {
        #[cfg(target_os = "windows")]
        {
            platform::configure_windows_audio(&host)?
        }

        #[cfg(target_os = "linux")]
        {
            platform::configure_linux_audio(&host)?
        }

        #[cfg(target_os = "macos")]
        {
            platform::configure_macos_audio(&host)?
        }
    };

    // Add any additional devices from the default host (non-Windows only;
    // on Windows, configure_windows_audio already enumerates all WASAPI input and output devices
    // with their exact DeviceType, and blindly marking remaining devices as Output can misclassify inputs).
    #[cfg(not(target_os = "windows"))]
    if let Ok(other_devices) = host.devices() {
        for device in other_devices {
            if let Ok(name) = device_name(&device) {
                if !devices.iter().any(|d| d.name == name) {
                    devices.push(AudioDevice::new(name, DeviceType::Output));
                }
            }
        }
    }

    Ok(devices)
}

/// Trigger audio permission request on platforms that require it
/// Returns Ok(true) if permission is granted, Ok(false) if denied, Err if something went wrong
pub fn trigger_audio_permission() -> Result<bool> {
    use log::info;

    #[cfg(target_os = "windows")]
    {
        platform::init_windows_audio_session_keeper();
        platform::ensure_windows_audio_session_volume(false);
    }

    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            info!("[trigger_audio_permission] No default input device found - permission likely denied");
            return Ok(false);
        }
    };

    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            info!("[trigger_audio_permission] Failed to get input config: {} - permission likely denied", e);
            return Ok(false);
        }
    };

    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    fn build_noop<T: cpal::SizedSample>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
    ) -> std::result::Result<cpal::Stream, cpal::Error> {
        device.build_input_stream(
            config,
            |_data: &[T], _: &cpal::InputCallbackInfo| {
                // Do nothing, we just want to trigger the permission request
            },
            |err| error!("Error in audio stream: {}", err),
            None,
        )
    }

    // Build and start an input stream to trigger the permission request
    let stream_result = match sample_format {
        cpal::SampleFormat::F32 => build_noop::<f32>(&device, stream_config),
        cpal::SampleFormat::I16 => build_noop::<i16>(&device, stream_config),
        cpal::SampleFormat::I32 => build_noop::<i32>(&device, stream_config),
        cpal::SampleFormat::I24 => build_noop::<cpal::I24>(&device, stream_config),
        cpal::SampleFormat::U16 => build_noop::<u16>(&device, stream_config),
        cpal::SampleFormat::U8 => build_noop::<u8>(&device, stream_config),
        cpal::SampleFormat::I8 => build_noop::<i8>(&device, stream_config),
        cpal::SampleFormat::U32 => build_noop::<u32>(&device, stream_config),
        cpal::SampleFormat::U24 => build_noop::<cpal::U24>(&device, stream_config),
        cpal::SampleFormat::F64 => build_noop::<f64>(&device, stream_config),
        _ => build_noop::<f32>(&device, stream_config),
    };

    let stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            info!("[trigger_audio_permission] Failed to build input stream: {} - permission likely denied", e);
            return Ok(false);
        }
    };

    // Start the stream to actually trigger the permission dialog
    if let Err(e) = stream.play() {
        info!("[trigger_audio_permission] Failed to play stream: {} - permission likely denied", e);
        return Ok(false);
    }

    // Sleep briefly to allow the permission dialog to appear and for stream to actually work
    std::thread::sleep(std::time::Duration::from_millis(500));

    // If we got here, permission was granted
    info!("[trigger_audio_permission] Stream played successfully - permission granted");

    // Stop the stream
    drop(stream);

    #[cfg(target_os = "windows")]
    {
        platform::ensure_windows_audio_session_volume(false);
    }

    Ok(true)
}
