#include <windows.h>
#include <d3d11.h>
#include <d3d10.h>
#include <dxgi.h>
#include <d3dcompiler.h>
#include <wrl/client.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <deque>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

#include "dynlink_cuda.h"
#include "dynlink_nvcuvid.h"

using Microsoft::WRL::ComPtr;

namespace {
void checked(HRESULT result, const char* operation) {
  if (FAILED(result)) {
    char text[160];
    std::snprintf(text, sizeof(text), "%s failed (0x%08lx)", operation,
                  static_cast<unsigned long>(result));
    throw std::runtime_error(text);
  }
}

void checked(CUresult result, const char* operation) {
  if (result != CUDA_SUCCESS) {
    throw std::runtime_error(std::string(operation) + " failed (CUDA " +
                             std::to_string(static_cast<int>(result)) + ")");
  }
}

void message(char* output, size_t capacity, const char* text) noexcept {
  if (output && capacity) {
    std::snprintf(output, capacity, "%s", text);
  }
}

template <typename Function>
Function* symbol(HMODULE library, const char* name) {
  auto* result = reinterpret_cast<Function*>(GetProcAddress(library, name));
  if (!result) throw std::runtime_error(std::string("Missing driver API: ") + name);
  return result;
}

struct Driver {
  HMODULE cuda = nullptr;
  HMODULE video = nullptr;
  tcuInit* init = nullptr;
  tcuD3D11GetDevice* device = nullptr;
  tcuCtxCreate_v2* create_context = nullptr;
  tcuCtxDestroy_v2* destroy_context = nullptr;
  tcuCtxPushCurrent_v2* push_context = nullptr;
  tcuCtxPopCurrent_v2* pop_context = nullptr;
  tcuGraphicsD3D11RegisterResource* register_texture = nullptr;
  tcuGraphicsUnregisterResource* unregister_texture = nullptr;
  tcuGraphicsMapResources* map_textures = nullptr;
  tcuGraphicsUnmapResources* unmap_textures = nullptr;
  tcuGraphicsSubResourceGetMappedArray* texture_array = nullptr;
  tcuMemcpy2D_v2* copy = nullptr;
  tcuvidGetDecoderCaps* capabilities = nullptr;
  tcuvidCreateDecoder* create_decoder = nullptr;
  tcuvidDestroyDecoder* destroy_decoder = nullptr;
  tcuvidDecodePicture* decode = nullptr;
  tcuvidMapVideoFrame* map_picture = nullptr;
  tcuvidUnmapVideoFrame* unmap_picture = nullptr;
  tcuvidCreateVideoParser* create_parser = nullptr;
  tcuvidDestroyVideoParser* destroy_parser = nullptr;
  tcuvidParseVideoData* parse = nullptr;

  Driver() {
    cuda = LoadLibraryExW(L"nvcuda.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    video = LoadLibraryExW(L"nvcuvid.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!cuda || !video) {
      if (video) FreeLibrary(video);
      if (cuda) FreeLibrary(cuda);
      cuda = video = nullptr;
      throw std::runtime_error("The installed NVIDIA NVDEC driver is unavailable");
    }
    try {
      init = symbol<tcuInit>(cuda, "cuInit");
      device = symbol<tcuD3D11GetDevice>(cuda, "cuD3D11GetDevice");
      create_context = symbol<tcuCtxCreate_v2>(cuda, "cuCtxCreate_v2");
      destroy_context = symbol<tcuCtxDestroy_v2>(cuda, "cuCtxDestroy_v2");
      push_context = symbol<tcuCtxPushCurrent_v2>(cuda, "cuCtxPushCurrent_v2");
      pop_context = symbol<tcuCtxPopCurrent_v2>(cuda, "cuCtxPopCurrent_v2");
      register_texture = symbol<tcuGraphicsD3D11RegisterResource>(cuda, "cuGraphicsD3D11RegisterResource");
      unregister_texture = symbol<tcuGraphicsUnregisterResource>(cuda, "cuGraphicsUnregisterResource");
      map_textures = symbol<tcuGraphicsMapResources>(cuda, "cuGraphicsMapResources");
      unmap_textures = symbol<tcuGraphicsUnmapResources>(cuda, "cuGraphicsUnmapResources");
      texture_array = symbol<tcuGraphicsSubResourceGetMappedArray>(cuda, "cuGraphicsSubResourceGetMappedArray");
      copy = symbol<tcuMemcpy2D_v2>(cuda, "cuMemcpy2D_v2");
      capabilities = symbol<tcuvidGetDecoderCaps>(video, "cuvidGetDecoderCaps");
      create_decoder = symbol<tcuvidCreateDecoder>(video, "cuvidCreateDecoder");
      destroy_decoder = symbol<tcuvidDestroyDecoder>(video, "cuvidDestroyDecoder");
      decode = symbol<tcuvidDecodePicture>(video, "cuvidDecodePicture");
      map_picture = symbol<tcuvidMapVideoFrame>(video, "cuvidMapVideoFrame64");
      unmap_picture = symbol<tcuvidUnmapVideoFrame>(video, "cuvidUnmapVideoFrame64");
      create_parser = symbol<tcuvidCreateVideoParser>(video, "cuvidCreateVideoParser");
      destroy_parser = symbol<tcuvidDestroyVideoParser>(video, "cuvidDestroyVideoParser");
      parse = symbol<tcuvidParseVideoData>(video, "cuvidParseVideoData");
    } catch (...) {
      FreeLibrary(video);
      FreeLibrary(cuda);
      video = cuda = nullptr;
      throw;
    }
  }

  ~Driver() {
    if (video) FreeLibrary(video);
    if (cuda) FreeLibrary(cuda);
  }
};

struct ContextScope {
  Driver& driver;
  explicit ContextScope(Driver& driver, CUcontext context) : driver(driver) {
    checked(driver.push_context(context), "cuCtxPushCurrent");
  }
  ~ContextScope() {
    CUcontext previous = nullptr;
    driver.pop_context(&previous);
  }
};

struct Output {
  ComPtr<ID3D11Texture2D> texture;
};

struct Frame {
  std::shared_ptr<Output> output;
  ComPtr<ID3D11Device> device;
  ComPtr<ID3D11DeviceContext> context;
  uint32_t width = 0;
  uint32_t height = 0;
  int64_t timestamp = 0;
};

const char* shader_source = R"(
Texture2D<float> luma : register(t0);
Texture2D<float> blue : register(t1);
Texture2D<float> red : register(t2);
SamplerState sample_state : register(s0);
cbuffer Conversion : register(b0) { float4 factors; };
struct Vertex { float4 position : SV_Position; float2 uv : TEXCOORD0; };
Vertex vertex(uint index : SV_VertexID) {
  Vertex output;
  output.uv = float2((index << 1) & 2, index & 2);
  output.position = float4(output.uv * float2(2, -2) + float2(-1, 1), 0, 1);
  return output;
}
float4 pixel(Vertex input) : SV_Target {
  float y = (luma.Sample(sample_state, input.uv) * 255 - factors.x) * factors.y;
  float cb = (blue.Sample(sample_state, input.uv) * 255 - 128) * factors.z;
  float cr = (red.Sample(sample_state, input.uv) * 255 - 128) * factors.z;
  float3 rgb = float3(y + 1.5748 * cr, y - 0.1873242729306488 * cb - 0.4681242729306488 * cr, y + 1.8556 * cb);
  return float4(saturate(rgb / 255), 1);
}
)";

struct Decoder {
  Driver driver;
  ComPtr<ID3D11Device> device;
  ComPtr<ID3D11DeviceContext> context;
  CUcontext cuda_context = nullptr;
  CUvideoparser parser = nullptr;
  CUvideodecoder decoder = nullptr;
  uint32_t width = 0;
  uint32_t height = 0;
  uint32_t coded_width = 0;
  uint32_t coded_height = 0;
  unsigned int surface_count = 0;
  std::array<int, 4> crop{};
  bool full_range = true;
  std::string failure;
  std::array<ComPtr<ID3D11Texture2D>, 3> planes;
  std::array<ComPtr<ID3D11ShaderResourceView>, 3> views;
  std::array<CUgraphicsResource, 3> resources{};
  ComPtr<ID3D11VertexShader> vertex_shader;
  ComPtr<ID3D11PixelShader> pixel_shader;
  ComPtr<ID3D11SamplerState> sampler;
  ComPtr<ID3D11Buffer> constants;
  std::vector<std::shared_ptr<Output>> pool;
  std::deque<std::unique_ptr<Frame>> frames;

  explicit Decoder(uint32_t adapter_index) {
    checked(driver.init(0), "cuInit");
    ComPtr<IDXGIFactory1> factory;
    checked(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1");
    ComPtr<IDXGIAdapter1> adapter;
    checked(factory->EnumAdapters1(adapter_index, &adapter), "EnumAdapters1");
    CUdevice cuda_device = 0;
    checked(driver.device(&cuda_device, adapter.Get()), "cuD3D11GetDevice");
    D3D_FEATURE_LEVEL requested[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    checked(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                             D3D11_CREATE_DEVICE_VIDEO_SUPPORT, requested, 2,
                             D3D11_SDK_VERSION, &device, nullptr, &context),
            "D3D11CreateDevice");
    ComPtr<ID3D10Multithread> protection;
    checked(context.As(&protection), "ID3D10Multithread");
    protection->SetMultithreadProtected(TRUE);
    checked(driver.create_context(&cuda_context, 0, cuda_device), "cuCtxCreate");
    try {
      CUVIDDECODECAPS caps{};
      caps.eCodecType = cudaVideoCodec_HEVC;
      caps.eChromaFormat = cudaVideoChromaFormat_444;
      checked(driver.capabilities(&caps), "cuvidGetDecoderCaps");
      if (!caps.bIsSupported || !(caps.nOutputFormatMask & (1 << cudaVideoSurfaceFormat_YUV444))) {
        throw std::runtime_error("NVDEC does not support 8-bit HEVC 4:4:4 output");
      }
      new_parser();
      CUcontext previous = nullptr;
      checked(driver.pop_context(&previous), "cuCtxPopCurrent");
    } catch (...) {
      if (parser) driver.destroy_parser(parser);
      driver.destroy_context(cuda_context);
      cuda_context = nullptr;
      throw;
    }
  }

  ~Decoder() {
    if (cuda_context) {
      try {
        ContextScope scope(driver, cuda_context);
        if (parser) driver.destroy_parser(parser);
        if (decoder) driver.destroy_decoder(decoder);
        for (auto resource : resources) {
          if (resource) driver.unregister_texture(resource);
        }
      } catch (...) {
      }
      driver.destroy_context(cuda_context);
    }
  }

  void new_parser() {
    CUVIDPARSERPARAMS parameters{};
    parameters.CodecType = cudaVideoCodec_HEVC;
    parameters.ulMaxNumDecodeSurfaces = 32;
    parameters.ulMaxDisplayDelay = 0;
    parameters.pUserData = this;
    parameters.pfnSequenceCallback = sequence_callback;
    parameters.pfnDecodePicture = decode_callback;
    parameters.pfnDisplayPicture = display_callback;
    checked(driver.create_parser(&parser, &parameters), "cuvidCreateVideoParser");
  }

  void reset() {
    ContextScope scope(driver, cuda_context);
    checked(driver.destroy_parser(parser), "cuvidDestroyVideoParser");
    parser = nullptr;
    frames.clear();
    failure.clear();
    new_parser();
  }

  static int CUDAAPI sequence_callback(void* opaque, CUVIDEOFORMAT* format) noexcept {
    auto* self = static_cast<Decoder*>(opaque);
    try { return self->sequence(*format); }
    catch (const std::exception& error) { self->failure = error.what(); return 0; }
    catch (...) { self->failure = "NVDEC sequence callback failed"; return 0; }
  }

  static int CUDAAPI decode_callback(void* opaque, CUVIDPICPARAMS* picture) noexcept {
    auto* self = static_cast<Decoder*>(opaque);
    try { checked(self->driver.decode(self->decoder, picture), "cuvidDecodePicture"); return 1; }
    catch (const std::exception& error) { self->failure = error.what(); return 0; }
    catch (...) { self->failure = "NVDEC decode callback failed"; return 0; }
  }

  static int CUDAAPI display_callback(void* opaque, CUVIDPARSERDISPINFO* picture) noexcept {
    auto* self = static_cast<Decoder*>(opaque);
    if (!picture) return 1;
    try { self->display(*picture); return 1; }
    catch (const std::exception& error) { self->failure = error.what(); return 0; }
    catch (...) { self->failure = "NVDEC display callback failed"; return 0; }
  }

  int sequence(const CUVIDEOFORMAT& format) {
    if (format.codec != cudaVideoCodec_HEVC || format.chroma_format != cudaVideoChromaFormat_444 ||
        format.bit_depth_luma_minus8 != 0 || format.bit_depth_chroma_minus8 != 0) {
      throw std::runtime_error("NVDEC path requires 8-bit HEVC 4:4:4; use software fallback");
    }
    const int visible_width = format.display_area.right - format.display_area.left;
    const int visible_height = format.display_area.bottom - format.display_area.top;
    if (visible_width <= 0 || visible_height <= 0 || visible_width > 8192 || visible_height > 8192 ||
        format.coded_width == 0 || format.coded_height == 0 || format.coded_width > 8192 || format.coded_height > 8192 ||
        static_cast<uint64_t>(format.coded_width) * format.coded_height > 16777216) {
      throw std::runtime_error("NVDEC picture dimensions exceed the input limit");
    }
    full_range = format.video_signal_description.video_full_range_flag != 0;
    const std::array<int, 4> next_crop = {format.display_area.left, format.display_area.top,
                                         format.display_area.right, format.display_area.bottom};
    if (decoder && coded_width == format.coded_width && coded_height == format.coded_height &&
        crop == next_crop && surface_count >= format.min_num_decode_surfaces) {
      return static_cast<int>(surface_count);
    }
    if (decoder) {
      checked(driver.destroy_decoder(decoder), "cuvidDestroyDecoder");
      decoder = nullptr;
    }
    for (auto& resource : resources) {
      if (resource) checked(driver.unregister_texture(resource), "cuGraphicsUnregisterResource");
      resource = nullptr;
    }
    views = {};
    planes = {};
    pool.clear();
    width = static_cast<uint32_t>(visible_width);
    height = static_cast<uint32_t>(visible_height);
    coded_width = format.coded_width;
    coded_height = format.coded_height;
    crop = next_crop;
    const unsigned int surfaces = std::clamp<unsigned int>(format.min_num_decode_surfaces + 3, 12, 32);
    surface_count = surfaces;
    if (static_cast<uint64_t>(format.coded_width) * format.coded_height * surfaces * 3 > 1024ULL * 1024 * 1024) {
      throw std::runtime_error("NVDEC reference surfaces exceed the memory budget");
    }
    CUVIDDECODECREATEINFO information{};
    information.ulWidth = format.coded_width;
    information.ulHeight = format.coded_height;
    information.ulNumDecodeSurfaces = surfaces;
    information.CodecType = format.codec;
    information.ChromaFormat = format.chroma_format;
    information.ulCreationFlags = cudaVideoCreate_PreferCUVID;
    information.display_area.left = static_cast<short>(format.display_area.left);
    information.display_area.top = static_cast<short>(format.display_area.top);
    information.display_area.right = static_cast<short>(format.display_area.right);
    information.display_area.bottom = static_cast<short>(format.display_area.bottom);
    information.OutputFormat = cudaVideoSurfaceFormat_YUV444;
    information.DeinterlaceMode = cudaVideoDeinterlaceMode_Weave;
    information.ulTargetWidth = width;
    information.ulTargetHeight = height;
    information.ulNumOutputSurfaces = 2;
    information.target_rect.right = static_cast<short>(width);
    information.target_rect.bottom = static_cast<short>(height);
    checked(driver.create_decoder(&decoder, &information), "cuvidCreateDecoder");
    D3D11_TEXTURE2D_DESC texture{};
    texture.Width = width;
    texture.Height = height;
    texture.MipLevels = texture.ArraySize = texture.SampleDesc.Count = 1;
    texture.Format = DXGI_FORMAT_R8_UNORM;
    texture.Usage = D3D11_USAGE_DEFAULT;
    texture.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    for (size_t plane = 0; plane < planes.size(); ++plane) {
      checked(device->CreateTexture2D(&texture, nullptr, &planes[plane]), "CreateTexture2D(YUV)");
      checked(device->CreateShaderResourceView(planes[plane].Get(), nullptr, &views[plane]), "CreateShaderResourceView");
      checked(driver.register_texture(&resources[plane], planes[plane].Get(), 0), "cuGraphicsD3D11RegisterResource");
    }
    if (!vertex_shader) {
      ComPtr<ID3DBlob> vertex;
      ComPtr<ID3DBlob> pixel;
      ComPtr<ID3DBlob> errors;
      checked(D3DCompile(shader_source, std::strlen(shader_source), nullptr, nullptr, nullptr,
                         "vertex", "vs_5_0", D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &vertex, &errors), "Compile video vertex shader");
      checked(D3DCompile(shader_source, std::strlen(shader_source), nullptr, nullptr, nullptr,
                         "pixel", "ps_5_0", D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &pixel, &errors), "Compile YUV conversion shader");
      checked(device->CreateVertexShader(vertex->GetBufferPointer(), vertex->GetBufferSize(), nullptr, &vertex_shader), "CreateVertexShader");
      checked(device->CreatePixelShader(pixel->GetBufferPointer(), pixel->GetBufferSize(), nullptr, &pixel_shader), "CreatePixelShader");
      D3D11_SAMPLER_DESC sampling{};
      sampling.Filter = D3D11_FILTER_MIN_MAG_MIP_POINT;
      sampling.AddressU = sampling.AddressV = sampling.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
      sampling.MaxLOD = D3D11_FLOAT32_MAX;
      checked(device->CreateSamplerState(&sampling, &sampler), "CreateSamplerState");
      D3D11_BUFFER_DESC buffer{};
      buffer.ByteWidth = 16;
      buffer.Usage = D3D11_USAGE_DEFAULT;
      buffer.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
      checked(device->CreateBuffer(&buffer, nullptr, &constants), "CreateBuffer(conversion)");
    }
    return static_cast<int>(surfaces);
  }

  void display(const CUVIDPARSERDISPINFO& picture) {
    std::shared_ptr<Output> output;
    for (auto& candidate : pool) {
      if (candidate.use_count() == 1) { output = candidate; break; }
    }
    if (!output) {
      if (pool.size() >= 6) return;
      output = std::make_shared<Output>();
      D3D11_TEXTURE2D_DESC texture{};
      texture.Width = width;
      texture.Height = height;
      texture.MipLevels = texture.ArraySize = texture.SampleDesc.Count = 1;
      texture.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
      texture.Usage = D3D11_USAGE_DEFAULT;
      texture.BindFlags = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;
      texture.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
      checked(device->CreateTexture2D(&texture, nullptr, &output->texture), "CreateTexture2D(RGBA)");
      pool.push_back(output);
    }
    CUVIDPROCPARAMS processing{};
    processing.progressive_frame = picture.progressive_frame;
    processing.top_field_first = picture.top_field_first;
    processing.unpaired_field = picture.repeat_first_field < 0;
    CUdeviceptr mapped = 0;
    unsigned int pitch = 0;
    checked(driver.map_picture(decoder, picture.picture_index, &mapped, &pitch, &processing), "cuvidMapVideoFrame");
    bool textures_mapped = false;
    try {
      checked(driver.map_textures(3, resources.data(), nullptr), "cuGraphicsMapResources");
      textures_mapped = true;
      for (size_t plane = 0; plane < resources.size(); ++plane) {
        CUarray array = nullptr;
        checked(driver.texture_array(&array, resources[plane], 0, 0), "cuGraphicsSubResourceGetMappedArray");
        CUDA_MEMCPY2D copy{};
        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        copy.srcDevice = mapped + static_cast<CUdeviceptr>(plane) * pitch * height;
        copy.srcPitch = pitch;
        copy.dstMemoryType = CU_MEMORYTYPE_ARRAY;
        copy.dstArray = array;
        copy.WidthInBytes = width;
        copy.Height = height;
        checked(driver.copy(&copy), "cuMemcpy2D(YUV texture)");
      }
      checked(driver.unmap_textures(3, resources.data(), nullptr), "cuGraphicsUnmapResources");
      textures_mapped = false;
      checked(driver.unmap_picture(decoder, mapped), "cuvidUnmapVideoFrame");
      mapped = 0;
    } catch (...) {
      if (textures_mapped) driver.unmap_textures(3, resources.data(), nullptr);
      if (mapped) driver.unmap_picture(decoder, mapped);
      throw;
    }
    ComPtr<ID3D11RenderTargetView> target;
    checked(device->CreateRenderTargetView(output->texture.Get(), nullptr, &target), "CreateRenderTargetView");
    float factors[] = {full_range ? 0.0f : 16.0f, full_range ? 1.0f : 255.0f / 219.0f,
                       full_range ? 1.0f : 255.0f / 224.0f, 0.0f};
    context->UpdateSubresource(constants.Get(), 0, nullptr, factors, 0, 0);
    D3D11_VIEWPORT viewport{0, 0, static_cast<float>(width), static_cast<float>(height), 0, 1};
    context->RSSetViewports(1, &viewport);
    context->OMSetRenderTargets(1, target.GetAddressOf(), nullptr);
    context->IASetInputLayout(nullptr);
    context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    context->VSSetShader(vertex_shader.Get(), nullptr, 0);
    context->PSSetShader(pixel_shader.Get(), nullptr, 0);
    context->PSSetSamplers(0, 1, sampler.GetAddressOf());
    context->PSSetConstantBuffers(0, 1, constants.GetAddressOf());
    ID3D11ShaderResourceView* sources[] = {views[0].Get(), views[1].Get(), views[2].Get()};
    context->PSSetShaderResources(0, 3, sources);
    context->Draw(3, 0);
    ID3D11ShaderResourceView* empty[] = {nullptr, nullptr, nullptr};
    context->PSSetShaderResources(0, 3, empty);
    context->OMSetRenderTargets(0, nullptr, nullptr);
    context->Flush();
    auto frame = std::make_unique<Frame>();
    frame->output = std::move(output);
    frame->device = device;
    frame->context = context;
    frame->width = width;
    frame->height = height;
    frame->timestamp = picture.timestamp;
    if (frames.size() == 3) frames.pop_front();
    frames.push_back(std::move(frame));
  }

  void push(const uint8_t* data, size_t length, int64_t timestamp, bool drain) {
    if (!failure.empty()) throw std::runtime_error(failure);
    if (length > 64 * 1024 * 1024) throw std::runtime_error("NVDEC input exceeds its resource limit");
    ContextScope scope(driver, cuda_context);
    CUVIDSOURCEDATAPACKET packet{};
    packet.flags = drain ? CUVID_PKT_ENDOFSTREAM : CUVID_PKT_TIMESTAMP | CUVID_PKT_ENDOFPICTURE;
    packet.payload = data;
    packet.payload_size = static_cast<tcu_ulong>(length);
    packet.timestamp = timestamp;
    const auto result = driver.parse(parser, &packet);
    if (!failure.empty()) throw std::runtime_error(failure);
    checked(result, "cuvidParseVideoData");
  }
};
}

extern "C" {
void* nhg_new(uint32_t adapter, char* error, size_t capacity) noexcept {
  try { return new Decoder(adapter); }
  catch (const std::exception& failure) { message(error, capacity, failure.what()); }
  catch (...) { message(error, capacity, "Cannot initialize NVDEC"); }
  return nullptr;
}

void nhg_free(void* decoder) noexcept { delete static_cast<Decoder*>(decoder); }

int nhg_push(void* decoder, const uint8_t* data, size_t length, int64_t timestamp,
             int drain, char* error, size_t capacity) noexcept {
  try { static_cast<Decoder*>(decoder)->push(data, length, timestamp, drain != 0); return 1; }
  catch (const std::exception& failure) { message(error, capacity, failure.what()); }
  catch (...) { message(error, capacity, "NVDEC failed"); }
  return 0;
}

int nhg_reset(void* decoder, char* error, size_t capacity) noexcept {
  try { static_cast<Decoder*>(decoder)->reset(); return 1; }
  catch (const std::exception& failure) { message(error, capacity, failure.what()); }
  catch (...) { message(error, capacity, "Cannot reset NVDEC parser"); }
  return 0;
}

void* nhg_take(void* decoder) noexcept {
  auto& frames = static_cast<Decoder*>(decoder)->frames;
  if (frames.empty()) return nullptr;
  auto frame = std::move(frames.front());
  frames.pop_front();
  return frame.release();
}

void nhg_frame_free(void* frame) noexcept { delete static_cast<Frame*>(frame); }
uint32_t nhg_frame_width(void* frame) noexcept { return static_cast<Frame*>(frame)->width; }
uint32_t nhg_frame_height(void* frame) noexcept { return static_cast<Frame*>(frame)->height; }

void* nhg_frame_texture(void* frame) noexcept {
  auto* texture = static_cast<Frame*>(frame)->output->texture.Get();
  texture->AddRef();
  return texture;
}

void* nhg_frame_device(void* frame) noexcept {
  auto* device = static_cast<Frame*>(frame)->device.Get();
  device->AddRef();
  return device;
}

int nhg_frame_read(void* opaque, uint8_t* bytes, size_t length,
                   char* error, size_t capacity) noexcept {
  try {
    auto& frame = *static_cast<Frame*>(opaque);
    if (length != static_cast<size_t>(frame.width) * frame.height * 4) {
      throw std::runtime_error("GPU readback buffer has an invalid size");
    }
    D3D11_TEXTURE2D_DESC description{};
    frame.output->texture->GetDesc(&description);
    description.Usage = D3D11_USAGE_STAGING;
    description.BindFlags = 0;
    description.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    description.MiscFlags = 0;
    ComPtr<ID3D11Texture2D> staging;
    checked(frame.device->CreateTexture2D(&description, nullptr, &staging), "CreateTexture2D(readback)");
    frame.context->CopyResource(staging.Get(), frame.output->texture.Get());
    D3D11_MAPPED_SUBRESOURCE mapped{};
    checked(frame.context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped), "Map GPU readback");
    for (uint32_t row = 0; row < frame.height; ++row) {
      std::memcpy(bytes + static_cast<size_t>(row) * frame.width * 4,
                  static_cast<uint8_t*>(mapped.pData) + static_cast<size_t>(row) * mapped.RowPitch,
                  static_cast<size_t>(frame.width) * 4);
    }
    frame.context->Unmap(staging.Get(), 0);
    return 1;
  } catch (const std::exception& failure) { message(error, capacity, failure.what()); }
  catch (...) { message(error, capacity, "GPU readback failed"); }
  return 0;
}
}