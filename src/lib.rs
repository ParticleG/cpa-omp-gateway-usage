#![deny(unsafe_op_in_unsafe_fn)]

mod plugin;
mod usage;

use std::ffi::{CStr, c_char, c_void};
use std::panic::catch_unwind;
use std::ptr;
use std::slice;

const VERSION: &str = match option_env!("PLUGIN_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

static PLUGIN: plugin::Plugin = plugin::Plugin::new();

#[repr(C)]
pub struct CliproxyBuffer {
    ptr: *mut c_void,
    len: usize,
}

type HostCall =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const u8, usize, *mut CliproxyBuffer) -> i32;
type FreeBuffer = unsafe extern "C" fn(*mut c_void, usize);
type PluginCall = unsafe extern "C" fn(*const c_char, *const u8, usize, *mut CliproxyBuffer) -> i32;
type PluginShutdown = unsafe extern "C" fn();

#[repr(C)]
pub struct CliproxyHostApi {
    abi_version: u32,
    host_ctx: *mut c_void,
    call: Option<HostCall>,
    free_buffer: Option<FreeBuffer>,
}

#[repr(C)]
pub struct CliproxyPluginApi {
    abi_version: u32,
    call: Option<PluginCall>,
    free_buffer: Option<FreeBuffer>,
    shutdown: Option<PluginShutdown>,
}

/// Initializes the CPA native ABI without depending on the host's language runtime.
///
/// # Safety
/// `plugin` must be null or point to a writable ABI 1 function table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cliproxy_plugin_init(
    _host: *const CliproxyHostApi,
    plugin: *mut CliproxyPluginApi,
) -> i32 {
    if plugin.is_null() {
        return 1;
    }
    // The host owns the output table; the callbacks remain valid until unload.
    unsafe {
        plugin.write(CliproxyPluginApi {
            abi_version: 1,
            call: Some(plugin_call),
            free_buffer: Some(plugin_free),
            shutdown: Some(plugin_shutdown),
        });
    }
    0
}

unsafe extern "C" fn plugin_call(
    method: *const c_char,
    request: *const u8,
    request_len: usize,
    response: *mut CliproxyBuffer,
) -> i32 {
    if response.is_null() {
        return 1;
    }
    unsafe {
        response.write(CliproxyBuffer {
            ptr: ptr::null_mut(),
            len: 0,
        });
    }
    if method.is_null() || request_len > 4 << 20 || (request.is_null() && request_len != 0) {
        return 1;
    }
    // CPA retains the method and input buffer for this synchronous call.
    let Ok(method) = (unsafe { CStr::from_ptr(method) }).to_str() else {
        return 1;
    };
    let request = if request_len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(request, request_len) }
    };
    // A Rust unwind must never cross the C ABI into the host.
    let Ok(body) = catch_unwind(|| PLUGIN.dispatch(method, request)) else {
        return 1;
    };
    let body = body.into_boxed_slice();
    let len = body.len();
    let data = Box::into_raw(body).cast::<u8>().cast::<c_void>();
    unsafe {
        response.write(CliproxyBuffer { ptr: data, len });
    }
    0
}

unsafe extern "C" fn plugin_free(data: *mut c_void, len: usize) {
    if !data.is_null() {
        // CPA returns the exact pointer/length pair once, using our allocator.
        let allocation = ptr::slice_from_raw_parts_mut(data.cast::<u8>(), len);
        unsafe { drop(Box::from_raw(allocation)) };
    }
}

unsafe extern "C" fn plugin_shutdown() {
    // Drop the pooled HTTP client and configuration before the image is unloaded.
    let _ = catch_unwind(|| PLUGIN.shutdown());
}
