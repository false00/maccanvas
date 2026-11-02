#include "de265.h"

#include <cstdint>
#include <new>

namespace {
constexpr int allocation_limit = -1;

struct Decoder {
  de265_decoder_context* context = nullptr;
  const de265_image_allocation* defaults = nullptr;
  int allocation_error = 0;
};

int get_buffer(de265_decoder_context* ctx, de265_image_spec* spec,
               de265_image* image, void* opaque) {
  auto* decoder = static_cast<Decoder*>(opaque);
  if (spec->width <= 0 || spec->height <= 0 || spec->width > 8192 ||
      spec->height > 8192 ||
      static_cast<int64_t>(spec->width) * spec->height > 16777216 ||
      spec->visible_width <= 0 || spec->visible_height <= 0 ||
      spec->visible_width > spec->width || spec->visible_height > spec->height) {
    decoder->allocation_error = allocation_limit;
    return 0;
  }
  return decoder->defaults->get_buffer(ctx, spec, image, nullptr);
}

void release_buffer(de265_decoder_context* ctx, de265_image* image, void* opaque) {
  auto* decoder = static_cast<Decoder*>(opaque);
  decoder->defaults->release_buffer(ctx, image, nullptr);
}

de265_image_allocation allocation = {get_buffer, release_buffer};

template <typename F> int checked(Decoder* decoder, F work) noexcept {
  try {
    const int result = work();
    return decoder->allocation_error ? decoder->allocation_error : result;
  } catch (const std::bad_alloc&) {
    return DE265_ERROR_OUT_OF_MEMORY;
  } catch (...) {
    return DE265_ERROR_UNSPECIFIED_DECODING_ERROR;
  }
}
}  // namespace

extern "C" {
void* nh_new(int* error) noexcept {
  *error = DE265_ERROR_OUT_OF_MEMORY;
  auto* decoder = new (std::nothrow) Decoder;
  if (!decoder) return nullptr;
  *error = checked(decoder, [&]() {
    decoder->context = de265_new_decoder();
    if (!decoder->context) return DE265_ERROR_LIBRARY_INITIALIZATION_FAILED;
    decoder->defaults = de265_get_default_image_allocation_functions();
    de265_set_image_allocation_functions(decoder->context, &allocation, decoder);
    de265_set_parameter_bool(decoder->context,
                            DE265_DECODER_PARAM_SUPPRESS_FAULTY_PICTURES, 1);
    de265_set_parameter_bool(decoder->context,
                            DE265_DECODER_PARAM_BOOL_SEI_CHECK_HASH, 1);
    return DE265_OK;
  });
  if (*error != DE265_OK) {
    delete decoder;
    return nullptr;
  }
  return decoder;
}

int nh_free(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  const int result = checked(decoder, [&]() {
    return de265_free_decoder(decoder->context);
  });
  delete decoder;
  return result;
}

int nh_push(void* opaque, const uint8_t* bytes, int length, int64_t pts) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return checked(decoder, [&]() {
    return de265_push_NAL(decoder->context, bytes, length, pts, nullptr);
  });
}

int nh_end_frame(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return checked(decoder, [&]() {
    de265_push_end_of_frame(decoder->context);
    return DE265_OK;
  });
}

int nh_flush(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return checked(decoder, [&]() { return de265_flush_data(decoder->context); });
}

int nh_decode(void* opaque, int* more) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return checked(decoder, [&]() { return de265_decode(decoder->context, more); });
}

int nh_warning(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return de265_get_warning(decoder->context);
}

const de265_image* nh_picture(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  return de265_peek_next_picture(decoder->context);
}

void nh_release(void* opaque) noexcept {
  auto* decoder = static_cast<Decoder*>(opaque);
  de265_release_next_picture(decoder->context);
}
}
