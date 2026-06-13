use libloading::{Library, Symbol};
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

pub struct KDMAPIBinds<'a> {
    is_kdmapi_available: Symbol<'a, unsafe extern "C" fn() -> bool>,
    initialize_kdmapi_stream: Symbol<'a, unsafe extern "C" fn() -> i32>,
    terminate_kdmapi_stream: Symbol<'a, unsafe extern "C" fn() -> i32>,
    send_direct_data: Symbol<'a, unsafe extern "C" fn(u32) -> u32>,
    is_stream_open: AtomicBool,
}

impl<'a> KDMAPIBinds<'a> {
    pub fn is_kdmapi_available(&self) -> bool {
        // Safety: FFI call to OmniMIDI, function pointer loaded from DLL
        unsafe { (self.is_kdmapi_available)() }
    }

    pub fn open_stream(&'a self) -> KDMAPIStream<'a> {
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
            KDMAPIStream { binds: &self }
        }
    }
}

fn load_kdmapi_lib() -> Library {
    // Safety: Library::new loads a shared library from the filesystem
    unsafe { Library::new(LIB_NAME).unwrap() }
}

fn load_kdmapi_binds<'a>(lib: &'a Library) -> KDMAPIBinds<'a> {
    // Safety: Loading symbols from a shared library via FFI; all symbols are expected to exist
    unsafe {
        KDMAPIBinds {
            is_kdmapi_available: lib.get(b"IsKDMAPIAvailable").unwrap(),
            initialize_kdmapi_stream: lib.get(b"InitializeKDMAPIStream").unwrap(),
            terminate_kdmapi_stream: lib.get(b"TerminateKDMAPIStream").unwrap(),
            send_direct_data: lib.get(b"SendDirectData").unwrap(),
            is_stream_open: AtomicBool::new(false),
        }
    }
}

pub struct KDMAPIStream<'a> {
    binds: &'a KDMAPIBinds<'a>,
}

impl<'a> KDMAPIStream<'a> {
    pub fn send_direct_data(&self, data: u32) -> u32 {
        // Safety: FFI call to OmniMIDI's send function
        unsafe { (self.binds.send_direct_data)(data) }
    }
}

impl<'a> Drop for KDMAPIStream<'a> {
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

static KDMAPI_LIB: OnceLock<Library> = OnceLock::new();
static KDMAPI_BINDS: OnceLock<KDMAPIBinds<'static>> = OnceLock::new();

pub fn get_kdmapi_lib() -> &'static Library {
    KDMAPI_LIB.get_or_init(|| load_kdmapi_lib())
}

pub fn get_kdmapi() -> &'static KDMAPIBinds<'static> {
    KDMAPI_BINDS.get_or_init(|| {
        let lib = get_kdmapi_lib();
        load_kdmapi_binds(lib)
    })
}
