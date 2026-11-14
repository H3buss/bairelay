pub mod audio_presence;
pub mod bc_opts;
pub mod bcmedia_dump;
pub mod camera;
pub mod camera_provider;
pub mod camera_tasks;
pub mod capabilities;
pub mod cli;
pub mod cli_convert;
pub mod config;
pub mod family_state;
pub mod grace_period;
pub mod hassio;
/// The wake-polling engine: one guarded question per wake announcement, so a
/// camera that woke by itself gets read instead of going back to sleep unseen.
///
/// Not driven on this branch: the UDP wake-announcement listener that feeds it
/// lives with the two-way-audio work, which needs the same session. The timings
/// it encodes — the 18 s a press took to arrive against the 250 ms a session
/// costs, the 249 s a held wake lock left the camera awake against 19 s without
/// it — were measured here and belong to this engine, so it waits here for its
/// caller instead of being rewritten over there.
#[allow(dead_code)]
pub(crate) mod polling;
pub(crate) mod published_text_guard;
pub mod local_time;
pub mod mqtt_dispatch;
pub mod mqtt_loop;
pub mod oneshot;
pub mod orchestrator;
pub mod preview_overlay;
pub mod preview_state;
pub mod push_listener;
pub mod settings;
pub mod run_support;
pub mod burst;
pub mod shutdown_signal;
pub mod startup_wake;
pub mod status_cache;
pub mod stream_source;
pub mod supervisor;
pub mod tls_load;
pub mod wake_events;
pub mod wake_lock;
pub mod watchdog;
pub mod webhook_listener;

/// Log capture for tests that assert on what is (and is not) logged.
#[cfg(test)]
pub(crate) mod test_logs {
	use std::sync::{Arc, Mutex};

	/// A `tracing` writer into a shared buffer.
	struct LogSink(Arc<Mutex<Vec<u8>>>);

	impl std::io::Write for LogSink {
		fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
			self.0.lock().unwrap().extend_from_slice(bytes);
			Ok(bytes.len())
		}

		fn flush(&mut self) -> std::io::Result<()> {
			Ok(())
		}
	}

	/// This thread's log output, captured while it lives.
	pub(crate) struct Captured {
		buf: Arc<Mutex<Vec<u8>>>,
		_guard: tracing::subscriber::DefaultGuard,
	}

	impl Captured {
		/// Everything captured so far.
		pub(crate) fn text(&self) -> String {
			String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
		}
	}

	/// Capture this thread's events at `level` and above, without ANSI
	/// or timestamps. Use a current-thread runtime.
	pub(crate) fn capture_at(level: tracing::Level) -> Captured {
		let buf = Arc::new(Mutex::new(Vec::new()));
		let sink = Arc::clone(&buf);
		let subscriber = tracing_subscriber::fmt()
			.with_ansi(false)
			.without_time()
			.with_max_level(level)
			.with_writer(move || LogSink(Arc::clone(&sink)))
			.finish();
		Captured {
			buf,
			_guard: tracing::subscriber::set_default(subscriber),
		}
	}
}
