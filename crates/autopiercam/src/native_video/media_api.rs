//! Resolve only system Media Foundation DLLs at encode time. Windows N without
//! the Media Feature Pack must still be able to run ordinary capture/settings.
use super::*;
use libloading::os::windows::{Library, Symbol};
use std::ffi::c_void;
use windows::{
    Win32::Media::MediaFoundation::*,
    core::{HRESULT, HSTRING, Interface},
};

pub(super) struct Api {
    platform: Library,
    readwrite: Library,
}
type Out = *mut *mut c_void;

// SAFETY: All callers use the documented factory's exact interface. A successful
// factory returns one owned COM reference; its module outlives every such value.
unsafe fn owned<T: Interface>(pointer: *mut c_void) -> Result<T> {
    ensure!(!pointer.is_null(), "native media factory returned null");
    Ok(unsafe { T::from_raw(pointer) })
}

impl Api {
    pub(super) fn load() -> Result<Self> {
        // LOAD_LIBRARY_SEARCH_SYSTEM32 excludes application/current/PATH DLLs.
        unsafe {
            Ok(Self {
                platform: Library::load_with_flags("mfplat.dll", 0x800).context("Windows Media Foundation is unavailable; install the Windows Media Feature Pack on N editions")?,
                readwrite: Library::load_with_flags("mfreadwrite.dll", 0x800).context("Windows Media Foundation read/write support is unavailable")?,
            })
        }
    }
    pub(super) fn startup(&self) -> Result<()> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, u32) -> HRESULT> =
                self.platform.get(b"MFStartup\0")?;
            f(MF_VERSION, MFSTARTUP_FULL).ok()?;
        }
        Ok(())
    }
    pub(super) fn shutdown(&self) -> Result<()> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn() -> HRESULT> =
                self.platform.get(b"MFShutdown\0")?;
            f().ok()?;
        }
        Ok(())
    }
    unsafe fn create<T: Interface>(&self, symbol: &[u8]) -> Result<T> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(Out) -> HRESULT> = self.platform.get(symbol)?;
            let mut pointer = std::ptr::null_mut();
            f(&mut pointer).ok()?;
            owned(pointer)
        }
    }
    pub(super) fn media_type(&self) -> Result<IMFMediaType> {
        unsafe { self.create(b"MFCreateMediaType\0") }
    }
    pub(super) fn sample(&self) -> Result<IMFSample> {
        unsafe { self.create(b"MFCreateSample\0") }
    }
    pub(super) fn attributes(&self) -> Result<IMFAttributes> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(Out, u32) -> HRESULT> =
                self.platform.get(b"MFCreateAttributes\0")?;
            let mut pointer = std::ptr::null_mut();
            f(&mut pointer, 4).ok()?;
            owned(pointer)
        }
    }
    pub(super) fn buffer(&self, size: u32) -> Result<IMFMediaBuffer> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(u32, Out) -> HRESULT> =
                self.platform.get(b"MFCreateMemoryBuffer\0")?;
            let mut pointer = std::ptr::null_mut();
            f(size, &mut pointer).ok()?;
            owned(pointer)
        }
    }
    pub(super) fn file(&self, path: &Path) -> Result<IMFByteStream> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(i32, i32, i32, *const u16, Out) -> HRESULT> =
                self.platform.get(b"MFCreateFile\0")?;
            let mut pointer = std::ptr::null_mut();
            f(
                MF_ACCESSMODE_WRITE.0,
                MF_OPENMODE_FAIL_IF_EXIST.0,
                MF_FILEFLAGS_NONE.0,
                HSTRING::from(path.as_os_str()).as_ptr(),
                &mut pointer,
            )
            .ok()?;
            owned(pointer)
        }
    }
    pub(super) fn writer(
        &self,
        stream: &IMFByteStream,
        attributes: &IMFAttributes,
    ) -> Result<IMFSinkWriter> {
        unsafe {
            let f: Symbol<
                unsafe extern "system" fn(*const u16, *mut c_void, *mut c_void, Out) -> HRESULT,
            > = self.readwrite.get(b"MFCreateSinkWriterFromURL\0")?;
            let mut pointer = std::ptr::null_mut();
            f(
                std::ptr::null(),
                stream.as_raw(),
                attributes.as_raw(),
                &mut pointer,
            )
            .ok()?;
            owned(pointer)
        }
    }
    pub(super) fn reader(
        &self,
        path: &Path,
        attributes: &IMFAttributes,
    ) -> Result<IMFSourceReader> {
        unsafe {
            let f: Symbol<unsafe extern "system" fn(*const u16, *mut c_void, Out) -> HRESULT> =
                self.readwrite.get(b"MFCreateSourceReaderFromURL\0")?;
            let mut pointer = std::ptr::null_mut();
            f(
                HSTRING::from(path.as_os_str()).as_ptr(),
                attributes.as_raw(),
                &mut pointer,
            )
            .ok()?;
            owned(pointer)
        }
    }
}
