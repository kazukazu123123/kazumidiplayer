use libloading::Library;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

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
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            panic!("KDMAPI stream is already open");
        }
        // Safety: FFI call to OmniMIDI; panics if stream initialization fails
        unsafe {
            let result = (self.initialize_kdmapi_stream)();
            if result == 0 {
                panic!("Failed to initialize KDMAPI stream");
            }
            KDMAPIStream { binds: self }
        }
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

pub struct KDMAPIStream {
    binds: &'static KDMAPIBinds,
}

impl KDMAPIStream {
    pub fn send_direct_data(&self, data: u32) -> u32 {
        // Safety: FFI call to OmniMIDI's send function
        unsafe { (self.binds.send_direct_data)(data) }
    }
}

impl Drop for KDMAPIStream {
    fn drop(&mut self) {
        // Safety: FFI call to close the OmniMIDI stream
        unsafe {
            (self.binds.terminate_kdmapi_stream)();
        }
        self.binds
            .is_stream_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
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
