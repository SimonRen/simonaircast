//! Real-time thread priority for audio work.
//!
//! On Windows, registers threads with MMCSS as "Pro Audio" so the scheduler
//! keeps them running on time under CPU load. Without it, a compile or game
//! using every core starved the sender for 229 ms — longer than the 100 ms
//! playout delay, i.e. an audible dropout. Elsewhere this is a no-op.

/// MMCSS registration for the current thread; reverted on drop.
pub struct ProAudioPriority(#[cfg(windows)] windows_sys::Win32::Foundation::HANDLE);

impl ProAudioPriority {
    /// Registers the current thread; `None` if the system refused.
    pub fn enter() -> Option<Self> {
        #[cfg(windows)]
        {
            let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut index = 0u32;
            // SAFETY: `task` is a NUL-terminated UTF-16 string that outlives the call.
            let handle = unsafe {
                windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                    task.as_ptr(),
                    &mut index,
                )
            };
            if handle.is_null() {
                tracing::debug!("MMCSS Pro Audio registration failed; staying at normal priority");
                None
            } else {
                Some(Self(handle))
            }
        }
        #[cfg(not(windows))]
        None
    }

    /// Registers the current thread for the rest of its life (e.g. runtime
    /// worker threads). Returns whether registration succeeded.
    pub fn for_thread_lifetime() -> bool {
        match Self::enter() {
            Some(guard) => {
                // Windows drops the registration when the thread exits.
                std::mem::forget(guard);
                true
            }
            None => false,
        }
    }
}

impl Drop for ProAudioPriority {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this thread.
        unsafe {
            windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0);
        }
    }
}
