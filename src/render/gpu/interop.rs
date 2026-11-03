use std::{
    ffi::{CStr, c_void},
    marker::PhantomData,
    num::NonZeroU32,
    rc::Rc,
    sync::Arc,
};

use windows::{
    Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
    core::Interface,
};

use crate::error::{Error, Result};

use super::nvdec::Frame;

type OpenDevice = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
type CloseDevice = unsafe extern "system" fn(*mut c_void) -> i32;
type Register = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> *mut c_void;
type Unregister = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type Lock = unsafe extern "system" fn(*mut c_void, i32, *const *mut c_void) -> i32;
type Generate = unsafe extern "system" fn(i32, *mut u32);
type Delete = unsafe extern "system" fn(i32, *const u32);
type GetInteger = unsafe extern "system" fn(u32, *mut i32);
type Bind = unsafe extern "system" fn(u32, u32);
type Parameter = unsafe extern "system" fn(u32, u32, i32);
type GetError = unsafe extern "system" fn() -> u32;

#[link(name = "opengl32")]
unsafe extern "system" {
    fn wglGetCurrentContext() -> *mut c_void;
}

fn load<T: Copy>(get: &dyn Fn(&CStr) -> *const c_void, name: &CStr) -> Result<T> {
    let address = get(name);
    if address as usize <= 3
        || address as usize == usize::MAX
        || std::mem::size_of::<T>() != std::mem::size_of_val(&address)
    {
        return Err(Error::Display(format!(
            "GPU texture sharing API is unavailable: {}",
            name.to_string_lossy()
        )));
    }
    Ok(unsafe { std::mem::transmute_copy(&address) })
}

struct Entry {
    texture: ID3D11Texture2D,
    image: slint::Image,
    object: *mut c_void,
    name: u32,
}

pub struct Interop {
    context: *mut c_void,
    open: OpenDevice,
    close: CloseDevice,
    register: Register,
    unregister: Unregister,
    lock: Lock,
    unlock: Lock,
    generate: Generate,
    delete: Delete,
    get_integer: GetInteger,
    bind: Bind,
    parameter: Parameter,
    error: GetError,
    handle: *mut c_void,
    device: Option<ID3D11Device>,
    entries: Vec<Entry>,
    locked: Option<usize>,
    held: Option<Arc<Frame>>,
    _thread: PhantomData<Rc<()>>,
}

impl Interop {
    /// # Safety
    /// The loader must return valid driver entry points for the current rendering context.
    pub unsafe fn new(get: &dyn Fn(&CStr) -> *const c_void) -> Result<Self> {
        let context = unsafe { wglGetCurrentContext() };
        if context.is_null() {
            return Err(Error::Display(
                "The renderer does not expose a native WGL context".into(),
            ));
        }
        Ok(Self {
            context,
            open: load(get, c"wglDXOpenDeviceNV")?,
            close: load(get, c"wglDXCloseDeviceNV")?,
            register: load(get, c"wglDXRegisterObjectNV")?,
            unregister: load(get, c"wglDXUnregisterObjectNV")?,
            lock: load(get, c"wglDXLockObjectsNV")?,
            unlock: load(get, c"wglDXUnlockObjectsNV")?,
            generate: load(get, c"glGenTextures")?,
            delete: load(get, c"glDeleteTextures")?,
            get_integer: load(get, c"glGetIntegerv")?,
            bind: load(get, c"glBindTexture")?,
            parameter: load(get, c"glTexParameteri")?,
            error: load(get, c"glGetError")?,
            handle: std::ptr::null_mut(),
            device: None,
            entries: Vec::new(),
            locked: None,
            held: None,
            _thread: PhantomData,
        })
    }

    pub fn image(&mut self, frame: &Arc<Frame>) -> Result<slint::Image> {
        if unsafe { wglGetCurrentContext() } != self.context {
            return Err(Error::Display(
                "GPU sharing requires its owning graphics context".into(),
            ));
        }
        self.finish();
        for _ in 0..16 {
            if unsafe { (self.error)() } == 0 {
                break;
            }
        }
        let device = frame.device();
        let texture = frame.texture();
        if self
            .device
            .as_ref()
            .is_some_and(|current| current.as_raw() != device.as_raw())
            || self.entries.len() >= 8
        {
            self.clear();
        }
        if self.device.is_none() {
            self.handle = unsafe { (self.open)(device.as_raw()) };
            if self.handle.is_null() {
                return Err(Error::Display(
                    "The OpenGL renderer cannot share this decoder's D3D11 device".into(),
                ));
            }
            self.device = Some(device);
        }
        let index = if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.texture.as_raw() == texture.as_raw())
        {
            index
        } else {
            let mut name = 0;
            unsafe { (self.generate)(1, &mut name) };
            let name = NonZeroU32::new(name)
                .ok_or_else(|| Error::Display("Cannot allocate a shared OpenGL texture".into()))?;
            let object =
                unsafe { (self.register)(self.handle, texture.as_raw(), name.get(), 0x0de1, 0) };
            if object.is_null() {
                unsafe { (self.delete)(1, &name.get()) };
                return Err(Error::Display(
                    "Cannot register the decoded GPU texture for presentation".into(),
                ));
            }
            let image = unsafe {
                slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                    name,
                    (frame.size.width, frame.size.height).into(),
                )
                .build()
            };
            self.entries.push(Entry {
                texture,
                image,
                object,
                name: name.get(),
            });
            self.entries.len() - 1
        };
        let entry = &self.entries[index];
        if unsafe { (self.lock)(self.handle, 1, &entry.object) } == 0 {
            return Err(Error::Display(
                "Cannot synchronize the decoded texture with the GPU renderer".into(),
            ));
        }
        self.locked = Some(index);
        self.held = Some(frame.clone());
        unsafe {
            let mut previous = 0;
            (self.get_integer)(0x8069, &mut previous);
            (self.bind)(0x0de1, entry.name);
            (self.parameter)(0x0de1, 0x2801, 0x2601);
            (self.parameter)(0x0de1, 0x2800, 0x2601);
            (self.parameter)(0x0de1, 0x2802, 0x812f);
            (self.parameter)(0x0de1, 0x2803, 0x812f);
            (self.bind)(0x0de1, previous as u32);
        }
        let error = unsafe { (self.error)() };
        if error != 0 {
            self.finish();
            return Err(Error::Display(format!(
                "Shared video texture initialization failed (OpenGL 0x{error:04x})"
            )));
        }
        Ok(entry.image.clone())
    }

    pub fn finish(&mut self) {
        if unsafe { wglGetCurrentContext() } != self.context {
            return;
        }
        if let Some(index) = self.locked.take() {
            unsafe { (self.unlock)(self.handle, 1, &self.entries[index].object) };
        }
        self.held = None;
    }

    pub fn clear(&mut self) {
        if unsafe { wglGetCurrentContext() } != self.context {
            return;
        }
        self.finish();
        for entry in self.entries.drain(..) {
            unsafe {
                (self.unregister)(self.handle, entry.object);
                (self.delete)(1, &entry.name);
            }
        }
        if !self.handle.is_null() {
            unsafe { (self.close)(self.handle) };
        }
        self.handle = std::ptr::null_mut();
        self.device = None;
    }
}

impl Drop for Interop {
    fn drop(&mut self) {
        if unsafe { wglGetCurrentContext() } == self.context {
            self.clear();
        }
    }
}
