use libloading::Library;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;

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

    pub fn open_stream(&'static self) -> KDMAPIStream {
        if self
            .is_stream_open
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("KDMAPI stream is already open");
        }
        // Safety: FFI call to OmniMIDI; panics if stream initialization fails.
        let result = unsafe { (self.initialize_kdmapi_stream)() };
        if result == 0 {
            self.is_stream_open.store(false, Ordering::SeqCst);
            panic!("Failed to initialize KDMAPI stream");
        }

        let (tx, rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = generation.clone();
        thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                match command {
                    KDMAPICommand::Data(data, event_generation) => {
                        // A file load advances the generation. Once a blocked FFI call
                        // returns, discard all output still queued for the old file.
                        if event_generation == worker_generation.load(Ordering::Acquire) {
                            // Safety: this dedicated worker is the only thread calling
                            // SendDirectData for this stream.
                            unsafe { (self.send_direct_data)(data) };
                        }
                    }
                    KDMAPICommand::Shutdown => break,
                }
            }
            // Safety: stream lifetime ends on the same dedicated FFI worker.
            unsafe { (self.terminate_kdmapi_stream)() };
            self.is_stream_open.store(false, Ordering::SeqCst);
        });

        KDMAPIStream { tx, generation }
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

enum KDMAPICommand {
    Data(u32, u64),
    Shutdown,
}

pub struct KDMAPIStream {
    tx: Sender<KDMAPICommand>,
    generation: Arc<AtomicU64>,
}

impl KDMAPIStream {
    /// Queue output without letting a blocked OmniMIDI call stall playback or loading.
    pub fn send_direct_data(&self, data: u32) -> u32 {
        let generation = self.generation.load(Ordering::Acquire);
        let _ = self.tx.send(KDMAPICommand::Data(data, generation));
        0
    }

    /// Make output already queued for the previous file obsolete.
    pub fn invalidate_pending(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for KDMAPIStream {
    fn drop(&mut self) {
        // Never wait here: OmniMIDI may currently be stuck inside SendDirectData.
        let _ = self.tx.send(KDMAPICommand::Shutdown);
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
