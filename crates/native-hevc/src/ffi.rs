use std::ffi::{c_char, c_int, c_void};

#[repr(C)]
pub struct Image {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    pub fn nh_new(error: *mut c_int) -> *mut c_void;
    pub fn nh_free(decoder: *mut c_void) -> c_int;
    pub fn nh_push(decoder: *mut c_void, bytes: *const u8, len: c_int, pts: i64) -> c_int;
    pub fn nh_end_frame(decoder: *mut c_void) -> c_int;
    pub fn nh_flush(decoder: *mut c_void) -> c_int;
    pub fn nh_decode(decoder: *mut c_void, more: *mut c_int) -> c_int;
    pub fn nh_warning(decoder: *mut c_void) -> c_int;
    pub fn nh_picture(decoder: *mut c_void) -> *const Image;
    pub fn nh_release(decoder: *mut c_void);

    pub fn de265_get_error_text(error: c_int) -> *const c_char;
    pub fn de265_get_image_width(image: *const Image, channel: c_int) -> c_int;
    pub fn de265_get_image_height(image: *const Image, channel: c_int) -> c_int;
    pub fn de265_get_chroma_format(image: *const Image) -> c_int;
    pub fn de265_get_bits_per_pixel(image: *const Image, channel: c_int) -> c_int;
    pub fn de265_get_image_plane(
        image: *const Image,
        channel: c_int,
        stride: *mut c_int,
    ) -> *const u8;
    pub fn de265_get_image_PTS(image: *const Image) -> i64;
    pub fn de265_get_image_full_range_flag(image: *const Image) -> c_int;
}
