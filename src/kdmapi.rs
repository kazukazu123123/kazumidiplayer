use libloading::Library;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "windows")]
const LIB_NAME: &str = "OmniMIDI.dll";
#[cfg(target_os = "linux")]
const LIB_NAME: &str = "libOmniMIDI.so";
#[cfg(target_os = "macos")]
const LIB_NAME: &str = "libOmniMIDI.dylib";
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
const LIB_NAME: &str = "OmniMIDI";

pub struct KDMAPIBinds {
    is_kdmapi_available: unsafe extern "C" fn() -> bool,
    initialize_kdmapi_stream: unsafe extern "C" fn() -> i32,
    terminate_kdmapi_stream: unsafe extern "C" fn() -> i32,
    send_direct_data: unsafe extern "C" fn(u32) -> u32,
    is_stream_open: AtomicBool,
}

impl KDMAPIBinds {
    pub fn is_kdmapi_available(&self) -> bool {
        // Safety: FFI call to OmniMIDI, function pointer loaded from DLL
        unsafe { (self.is_kdmapi_available)() }
    }

    /// `None` if a previous stream is still open or initialization fails.
    pub fn open_stream(&'static self) -> Option<KDMAPIStream> {
        if self
            .is_stream_open
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }
        // Safety: FFI call to OmniMIDI.
        let result = unsafe { (self.initialize_kdmapi_stream)() };
        if result == 0 {
            self.is_stream_open.store(false, Ordering::SeqCst);
            return None;
        }
        Some(KDMAPIStream {
            binds: self,
            closed: false,
        })
    }
}

fn load_kdmapi_lib() -> Option<Library> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join(LIB_NAME);
            // Safety: Library::new loads a shared library from the filesystem
            if let Ok(lib) = unsafe { Library::new(&bundled) } {
                return Some(lib);
            }
        }
    }
    // Safety: Library::new loads a shared library from the filesystem
    unsafe { Library::new(LIB_NAME).ok() }
}

fn load_kdmapi_binds(lib: &Library) -> Option<KDMAPIBinds> {
    // Safety: Loading symbols from a shared library via FFI; all symbols are expected to exist
    let is_kdmapi_available = unsafe { lib.get(b"IsKDMAPIAvailable") }.ok()?;
    let initialize_kdmapi_stream = unsafe { lib.get(b"InitializeKDMAPIStream") }.ok()?;
    let terminate_kdmapi_stream = unsafe { lib.get(b"TerminateKDMAPIStream") }.ok()?;
    let send_direct_data = unsafe { lib.get(b"SendDirectData") }.ok()?;
    Some(KDMAPIBinds {
        is_kdmapi_available: *is_kdmapi_available,
        initialize_kdmapi_stream: *initialize_kdmapi_stream,
        terminate_kdmapi_stream: *terminate_kdmapi_stream,
        send_direct_data: *send_direct_data,
        is_stream_open: AtomicBool::new(false),
    })
}

/// An open OmniMIDI stream. Only one exists at a time.
pub struct KDMAPIStream {
    binds: &'static KDMAPIBinds,
    closed: bool,
}

impl KDMAPIStream {
    pub fn send(&mut self, data: u32) -> u32 {
        // Safety: FFI call to an initialized stream; `&mut` excludes overlap.
        unsafe { (self.binds.send_direct_data)(data) }
    }

    pub fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        // Safety: `&mut` guarantees no send is in progress.
        unsafe { (self.binds.terminate_kdmapi_stream)() };
        self.binds.is_stream_open.store(false, Ordering::SeqCst);
    }
}

impl Drop for KDMAPIStream {
    fn drop(&mut self) {
        self.close();
    }
}

static KDMAPI: OnceLock<Option<KDMAPIBinds>> = OnceLock::new();

pub fn get_kdmapi() -> Option<&'static KDMAPIBinds> {
    KDMAPI
        .get_or_init(|| {
            let lib = load_kdmapi_lib()?;
            let binds = load_kdmapi_binds(&lib)?;
            // Leak the library so it lives for the program's lifetime. The function
            // pointers inside the binds reference the library's memory, so the library
            // must outlive the binds.
            Box::leak(Box::new(lib));
            Some(binds)
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_engine::{AudioEngine, OutputStream};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Mutex, MutexGuard};
    use std::thread::{self, ThreadId};
    use std::time::{Duration, Instant};

    const BLOCKED_MESSAGE: u32 = 0xFFFF_FFFF;
    const RESULT_MASK: u32 = 0x1234_5678;
    const TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Debug, PartialEq)]
    enum Call {
        Data(u32, ThreadId),
        Terminate,
    }

    struct MockDriver {
        calls: Sender<Call>,
        release: Option<mpsc::Receiver<()>>,
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static MOCK_DRIVER: Mutex<Option<MockDriver>> = Mutex::new(None);
    static MOCK_BINDS: KDMAPIBinds = KDMAPIBinds {
        is_kdmapi_available: mock_available,
        initialize_kdmapi_stream: mock_initialize,
        terminate_kdmapi_stream: mock_terminate,
        send_direct_data: mock_send,
        is_stream_open: AtomicBool::new(false),
    };

    unsafe extern "C" fn mock_available() -> bool {
        true
    }

    unsafe extern "C" fn mock_initialize() -> i32 {
        1
    }

    unsafe extern "C" fn mock_terminate() -> i32 {
        let guard = MOCK_DRIVER.lock().unwrap();
        let _ = guard.as_ref().unwrap().calls.send(Call::Terminate);
        1
    }

    unsafe extern "C" fn mock_send(data: u32) -> u32 {
        let release = {
            let mut guard = MOCK_DRIVER.lock().unwrap();
            let driver = guard.as_mut().unwrap();
            let _ = driver.calls.send(Call::Data(data, thread::current().id()));
            if data == BLOCKED_MESSAGE {
                driver.release.take()
            } else {
                None
            }
        };
        if let Some(release) = release {
            let _ = release.recv_timeout(TIMEOUT);
        }
        data ^ RESULT_MASK
    }

    struct MockEngine(KDMAPIStream);

    impl AudioEngine for MockEngine {
        const LABEL: &'static str = "mock";

        fn open() -> Result<Self, String> {
            MOCK_BINDS.open_stream().map(Self).ok_or_else(|| "busy".into())
        }

        fn send_midi_event(&mut self, data: u32) -> u32 {
            self.0.send(data)
        }

        fn close(&mut self) {
            self.0.close();
        }
    }

    struct TestStream {
        stream: Option<OutputStream>,
        calls: mpsc::Receiver<Call>,
        release: Option<Sender<()>>,
        _guard: MutexGuard<'static, ()>,
    }

    impl TestStream {
        fn new() -> Self {
            let guard = TEST_LOCK.lock().unwrap();
            let (tx_calls, calls) = mpsc::channel();
            let (release, rx_release) = mpsc::channel();
            *MOCK_DRIVER.lock().unwrap() = Some(MockDriver {
                calls: tx_calls,
                release: Some(rx_release),
            });
            Self {
                stream: Some(
                    OutputStream::open_engine::<MockEngine>()
                        .recv_timeout(TIMEOUT)
                        .unwrap()
                        .unwrap(),
                ),
                calls,
                release: Some(release),
                _guard: guard,
            }
        }

        fn next_call(&self) -> Call {
            self.calls.recv_timeout(TIMEOUT).unwrap()
        }
    }

    impl Drop for TestStream {
        fn drop(&mut self) {
            // Unblock the mock even when an assertion fails, then await shutdown
            // before the next test replaces the shared driver.
            self.release.take();
            self.stream.take();
            while let Ok(call) = self.calls.recv_timeout(TIMEOUT) {
                if call == Call::Terminate {
                    break;
                }
            }
            let deadline = Instant::now() + TIMEOUT;
            while MOCK_BINDS.is_stream_open.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::yield_now();
            }
        }
    }

    #[test]
    fn synchronous_sends_run_on_caller_and_return_driver_result() {
        let mut test = TestStream::new();
        let caller = thread::current().id();
        for data in 1..=3 {
            assert_eq!(
                test.stream
                    .as_mut()
                    .unwrap()
                    .send_direct_data_blocking(data),
                data ^ RESULT_MASK
            );
            assert_eq!(test.next_call(), Call::Data(data, caller));
        }
    }

    #[test]
    fn queued_and_direct_sends_keep_order_across_mode_changes() {
        let mut test = TestStream::new();
        let stream = test.stream.as_mut().unwrap();
        stream.send_direct_data(1);
        stream.send_direct_data(2);
        stream.send_direct_data_blocking(3);
        stream.send_direct_data_blocking(4);
        stream.send_direct_data(5);
        stream.send_direct_data_blocking(6);

        let caller = thread::current().id();
        for expected in 1..=6 {
            let Call::Data(data, thread_id) = test.next_call() else {
                panic!("stream terminated before sending all events");
            };
            assert_eq!(data, expected);
            assert_eq!(thread_id == caller, matches!(data, 3 | 4 | 6));
        }
    }

    #[test]
    fn invalidation_discards_old_queue_before_direct_send() {
        let mut test = TestStream::new();
        test.stream
            .as_ref()
            .unwrap()
            .send_direct_data(BLOCKED_MESSAGE);
        assert!(matches!(test.next_call(), Call::Data(BLOCKED_MESSAGE, _)));

        let stream = test.stream.as_mut().unwrap();
        stream.send_direct_data(2);
        stream.invalidate_pending();
        stream.send_direct_data(3);
        test.release.take().unwrap().send(()).unwrap();
        stream.send_direct_data_blocking(4);

        assert!(matches!(test.next_call(), Call::Data(3, _)));
        assert_eq!(test.next_call(), Call::Data(4, thread::current().id()));
        assert!(test.calls.try_recv().is_err());
    }

    #[test]
    fn dropping_stream_does_not_wait_for_blocked_queued_send() {
        let mut test = TestStream::new();
        test.stream
            .as_ref()
            .unwrap()
            .send_direct_data(BLOCKED_MESSAGE);
        assert!(matches!(test.next_call(), Call::Data(BLOCKED_MESSAGE, _)));

        let stream = test.stream.take().unwrap();
        let (tx_dropped, rx_dropped) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(stream);
            let _ = tx_dropped.send(());
        });
        let dropped = rx_dropped.recv_timeout(Duration::from_secs(1));
        test.release.take().unwrap().send(()).unwrap();
        dropper.join().unwrap();
        assert!(dropped.is_ok());
    }
}
