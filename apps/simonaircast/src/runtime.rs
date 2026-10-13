//! Process-level timing setup shared by the CLI and the tray app.

use audio_capture::priority::ProAudioPriority;

/// Builds the tokio runtime with every worker thread registered as MMCSS
/// "Pro Audio". Packet sending runs on these threads; at normal priority a busy
/// CPU (e.g. a compile) stalled it for 229 ms, past the playout delay. The
/// calling thread is registered too, since it drives the top-level future.
pub fn build() -> std::io::Result<tokio::runtime::Runtime> {
    ProAudioPriority::for_thread_lifetime();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_start(|| {
            ProAudioPriority::for_thread_lifetime();
        })
        .build()
}

/// Requests 1 ms Windows timer resolution while alive. The default 15.6 ms
/// tick makes the 8 ms packets go out in bursts, which forces a larger PC
/// buffer (= more latency).
pub struct HighResTimer;

impl HighResTimer {
    pub fn enter() -> Self {
        #[cfg(windows)]
        // SAFETY: plain winmm call, paired with timeEndPeriod in Drop.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for HighResTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: matches the timeBeginPeriod(1) in `enter`.
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}
