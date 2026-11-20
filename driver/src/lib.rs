//! SoundRouter AudioServerPlugIn: up to 16 loopback devices, each with a hidden engine companion.
//!
//! For each device V (what apps pick) there is a companion C (what the SoundRouter engine uses):
//! V.out -> ring A -> C.in, and C.out -> ring B -> V.in. Apps play into V and record from V; the
//! engine reads the apps' mix on C.in and writes the routed mix to C.out.
//!
//! Devices are created and removed at runtime through the custom `srcf` property on the plug-in
//! object (see `sr_core::driver_proto`). Every `extern "C"` entry point aborts on panic.
#![allow(non_upper_case_globals, non_snake_case)]

mod cf;
mod config;
mod io;
mod plugin;
mod props;
mod ring;
mod state;

use coreaudio_sys::*;
use std::ffi::c_void;
use std::ptr::null_mut;

struct Iface(AudioServerPlugInDriverInterface);
// SAFETY: the table is immutable and holds only function pointers.
unsafe impl Sync for Iface {}
static IFACE: Iface = Iface(AudioServerPlugInDriverInterface {
    _reserved: null_mut(),
    QueryInterface: Some(query_interface),
    AddRef: Some(add_ref),
    Release: Some(release),
    Initialize: Some(plugin::initialize),
    CreateDevice: Some(plugin::create_device),
    DestroyDevice: Some(plugin::destroy_device),
    AddDeviceClient: Some(plugin::device_client),
    RemoveDeviceClient: Some(plugin::device_client),
    PerformDeviceConfigurationChange: Some(plugin::perform_change),
    AbortDeviceConfigurationChange: Some(plugin::abort_change),
    HasProperty: Some(plugin::has_property),
    IsPropertySettable: Some(plugin::is_settable),
    GetPropertyDataSize: Some(plugin::get_size),
    GetPropertyData: Some(plugin::get_data),
    SetPropertyData: Some(plugin::set_data),
    StartIO: Some(io::start_io),
    StopIO: Some(io::stop_io),
    GetZeroTimeStamp: Some(io::zero_ts),
    WillDoIOOperation: Some(io::will_do),
    BeginIOOperation: Some(io::begin_end),
    DoIOOperation: Some(io::do_io),
    EndIOOperation: Some(io::begin_end),
});
struct IfacePtr(*mut AudioServerPlugInDriverInterface);
// SAFETY: points at the immutable static above.
unsafe impl Sync for IfacePtr {}
static IFACE_PTR: IfacePtr = IfacePtr(&IFACE.0 as *const _ as *mut _);

fn driver_ref() -> AudioServerPlugInDriverRef {
    &IFACE_PTR.0 as *const _ as AudioServerPlugInDriverRef
}

const TYPE_UUID: [u8; 16] = [
    0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB,
];
const DRIVER_UUID: [u8; 16] = [
    0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17,
];
const IUNKNOWN_UUID: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const E_NOINTERFACE: HRESULT = 0x8000_0004u32 as HRESULT;

fn uuid_bytes(u: CFUUIDBytes) -> [u8; 16] {
    [
        u.byte0, u.byte1, u.byte2, u.byte3, u.byte4, u.byte5, u.byte6, u.byte7, u.byte8, u.byte9,
        u.byte10, u.byte11, u.byte12, u.byte13, u.byte14, u.byte15,
    ]
}

/// CFPlugIn factory named in Info.plist `CFPlugInFactories`.
///
/// # Safety
/// Called by CFPlugIn with a valid (or null) CFUUID.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SoundRouter_Create(
    _alloc: CFAllocatorRef,
    type_uuid: CFUUIDRef,
) -> *mut c_void {
    if type_uuid.is_null() || uuid_bytes(unsafe { CFUUIDGetUUIDBytes(type_uuid) }) != TYPE_UUID {
        return null_mut();
    }
    driver_ref().cast()
}

unsafe extern "C" fn query_interface(d: *mut c_void, uuid: REFIID, out: *mut LPVOID) -> HRESULT {
    if d != driver_ref().cast() || out.is_null() {
        return kAudioHardwareBadObjectError as HRESULT;
    }
    let b = uuid_bytes(uuid);
    unsafe {
        if b == DRIVER_UUID || b == IUNKNOWN_UUID {
            *out = d;
            0
        } else {
            *out = null_mut();
            E_NOINTERFACE
        }
    }
}
// Static singleton: reference counting is a no-op.
unsafe extern "C" fn add_ref(_: *mut c_void) -> ULONG {
    1
}
unsafe extern "C" fn release(_: *mut c_void) -> ULONG {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_and_query_interface_refuse_what_is_not_theirs() {
        // SAFETY: CFUUIDBytes is 16 bytes, repr(C).
        let id = |b: [u8; 16]| unsafe { std::mem::transmute::<[u8; 16], CFUUIDBytes>(b) };
        let d: *mut c_void = driver_ref().cast();
        let bad = kAudioHardwareBadObjectError as HRESULT;
        unsafe {
            assert!(SoundRouter_Create(std::ptr::null(), std::ptr::null()).is_null());
            for (t, ok) in [
                (TYPE_UUID, true),
                (DRIVER_UUID, false),
                (IUNKNOWN_UUID, false),
            ] {
                let u = CFUUIDCreateFromUUIDBytes(std::ptr::null(), id(t));
                assert_eq!(SoundRouter_Create(std::ptr::null(), u) == d, ok);
                assert_eq!(SoundRouter_Create(std::ptr::null(), u).is_null(), !ok);
                CFRelease(u as CFTypeRef);
            }
            for (iid, want) in [
                (DRIVER_UUID, 0),
                (IUNKNOWN_UUID, 0),
                (TYPE_UUID, E_NOINTERFACE),
            ] {
                let mut out: LPVOID = 1 as LPVOID;
                assert_eq!(query_interface(d, id(iid), &mut out), want);
                assert_eq!(out, if want == 0 { d } else { null_mut() });
            }
            let mut out = null_mut();
            assert_eq!(query_interface(null_mut(), id(DRIVER_UUID), &mut out), bad);
            assert_eq!(query_interface(d, id(DRIVER_UUID), null_mut()), bad);
            assert_eq!((add_ref(d), release(d), release(d)), (1, 1, 1)); // static singleton
        }
    }
}
