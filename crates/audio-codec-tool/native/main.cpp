// SPDX-License-Identifier: 0BSD
#include <array>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>
#ifdef _WIN32
#include <fcntl.h>
#include <io.h>
#endif
#include "aacdecoder_lib.h"
#include "licenses.h"

static bool read_bytes(unsigned char *bytes, size_t count) {
    return std::fread(bytes, 1, count, stdin) == count;
}

static bool write_bytes(const void *bytes, size_t count) {
    return std::fwrite(bytes, 1, count, stdout) == count;
}

static bool word(uint32_t value, unsigned count) {
    unsigned char bytes[4];
    for (unsigned i = 0; i < count; ++i) bytes[i] = (unsigned char)(value >> (8 * i));
    return write_bytes(bytes, count);
}

static bool wave_header(uint32_t rate, uint16_t channels) {
    return write_bytes("RIFF", 4) && word(0xffffffff, 4) && write_bytes("WAVEfmt ", 8) &&
        word(16, 4) && word(1, 2) && word(channels, 2) && word(rate, 4) &&
        word(rate * channels * 2, 4) && word(channels * 2, 2) && word(16, 2) &&
        write_bytes("data", 4) && word(0xffffffff, 4);
}

static int convert() {
    HANDLE_AACDECODER decoder = aacDecoder_Open(TT_MP4_LOAS, 1);
    if (!decoder) return 3;
    std::array<unsigned char, 8194> packet{};
    std::vector<INT_PCM> pcm(16384);
    bool header_written = false;
    int rate = 0, channels = 0;
    int result = 0;
    for (;;) {
        int first = std::fgetc(stdin);
        if (first == EOF) {
            if (std::ferror(stdin)) result = 4;
            break;
        }
        packet[0] = (unsigned char)first;
        if (!read_bytes(packet.data() + 1, 2) || packet[0] != 0x56 || (packet[1] & 0xe0) != 0xe0) {
            result = 5;
            break;
        }
        UINT size = UINT(((packet[1] & 31) << 8) | packet[2]);
        if (!size || !read_bytes(packet.data() + 3, size)) {
            result = 6;
            break;
        }
        size += 3;
        UINT valid = size;
        UCHAR *bytes = packet.data();
        auto status = aacDecoder_Fill(decoder, &bytes, &size, &valid);
        if (status != AAC_DEC_OK || valid) {
            result = 7;
            break;
        }
        for (unsigned frame = 0; frame < 16; ++frame) {
            status = aacDecoder_DecodeFrame(decoder, pcm.data(), int(pcm.size()), 0);
            if (status == AAC_DEC_NOT_ENOUGH_BITS) break;
            if (status != AAC_DEC_OK) {
                std::fprintf(stderr, "AAC decode error 0x%x\n", status);
                result = 8;
                break;
            }
            const CStreamInfo *info = aacDecoder_GetStreamInfo(decoder);
            if (!info || info->numChannels < 1 || info->numChannels > 8 ||
                info->sampleRate < 8000 || info->sampleRate > 96000 ||
                info->frameSize < 1 || info->frameSize > 2048 ||
                size_t(info->numChannels) * info->frameSize > pcm.size()) {
                result = 9;
                break;
            }
            if (!header_written) {
                rate = info->sampleRate;
                channels = info->numChannels;
                if (!wave_header(uint32_t(rate), uint16_t(channels))) {
                    result = 10;
                    break;
                }
                header_written = true;
            }
            if (rate != info->sampleRate || channels != info->numChannels) {
                result = 11;
                break;
            }
            size_t samples = size_t(info->frameSize) * channels;
            if (!write_bytes(pcm.data(), samples * sizeof(INT_PCM))) {
                result = 12;
                break;
            }
            std::memset(pcm.data(), 0, samples * sizeof(INT_PCM));
        }
        if (result) break;
    }
    aacDecoder_Close(decoder);
    return result;
}

int main(int argc, char **argv) {
    if (argc == 2 && std::strcmp(argv[1], "--version") == 0) {
        std::puts("aac-to-wav 0.1.0");
        return 0;
    }
    if (argc == 2 && std::strcmp(argv[1], "--licenses") == 0) {
        std::puts(kLicenses);
        return 0;
    }
    if (argc != 1) {
        std::fputs("Usage: aac-to-wav < input.loas > output.wav\n       aac-to-wav --version | --licenses\n", stderr);
        return 2;
    }
#ifdef _WIN32
    if (_setmode(_fileno(stdin), _O_BINARY) < 0 || _setmode(_fileno(stdout), _O_BINARY) < 0) return 13;
#endif
    std::setvbuf(stdout, nullptr, _IONBF, 0);
    int status = convert();
    if (status) std::fprintf(stderr, "Conversion failed (%d)\n", status);
    return status;
}
