/* SPDX-License-Identifier: AGPL-3.0-or-later */
#include <AudioToolbox/AudioToolbox.h>
#include <AudioToolbox/AudioCodec.h>
#include <AudioUnit/AudioComponent.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#define CHECK(call) do { \
    OSStatus status = (call); \
    if (status != noErr) { \
        fprintf(stderr, "%s: OSStatus %d (0x%08x)\n", #call, (int)status, (unsigned)status); \
        return 1; \
    } \
} while (0)

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    AudioComponentDescription description = {
        .componentType = kAudioEncoderComponentType,
        .componentSubType = kAudioFormatMPEG4AAC_ELD_SBR
    };
    AudioComponent component = AudioComponentFindNext(NULL, &description);
    if (!component) {
        fputs("No native AAC-ELD-SBR encoder component\n", stderr);
        return 3;
    }
    AudioComponentInstance instance = NULL;
    CHECK(AudioComponentInstanceNew(component, &instance));
    AudioStreamBasicDescription input = {
        .mSampleRate = 48000,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian,
        .mBytesPerPacket = 8, .mFramesPerPacket = 1,
        .mBytesPerFrame = 8, .mChannelsPerFrame = 2, .mBitsPerChannel = 32
    };
    AudioStreamBasicDescription output = {
        .mSampleRate = 48000, .mFormatID = kAudioFormatMPEG4AAC_ELD_SBR,
        .mFramesPerPacket = 480, .mChannelsPerFrame = 2
    };
    CHECK(AudioCodecSetProperty(instance, kAudioCodecPropertyCurrentInputFormat, sizeof(input), &input));
    CHECK(AudioCodecSetProperty(instance, kAudioCodecPropertyCurrentOutputFormat, sizeof(output), &output));
    UInt32 frames_per_packet = 480, bitrate = 80000;
    CHECK(AudioCodecSetProperty(instance, kAudioCodecPropertyPacketFrameSize, sizeof(frames_per_packet), &frames_per_packet));
    CHECK(AudioCodecSetProperty(instance, kAudioCodecPropertyCurrentTargetBitRate, sizeof(bitrate), &bitrate));
    CHECK(AudioCodecInitialize(instance, &input, &output, NULL, 0));
    UInt32 size = sizeof(output);
    CHECK(AudioCodecGetProperty(instance, kAudioCodecPropertyCurrentOutputFormat, &size, &output));
    if (output.mSampleRate != 48000 || output.mChannelsPerFrame != 2 || output.mFramesPerPacket != 480) {
        fprintf(stderr, "Unexpected encoder format: %.0f Hz, %u channels, %u frames\n",
                output.mSampleRate, output.mChannelsPerFrame, output.mFramesPerPacket);
        return 4;
    }
    unsigned char cookie[4096];
    Boolean writable = false;
    size = 0;
    CHECK(AudioCodecGetPropertyInfo(instance, kAudioCodecPropertyMagicCookie, &size, &writable));
    if (size > sizeof(cookie)) return 5;
    CHECK(AudioCodecGetProperty(instance, kAudioCodecPropertyMagicCookie, &size, cookie));
    printf("Native codec cookie (%u bytes): ", size);
    for (UInt32 i = 0; i < size; ++i) printf("%02x", cookie[i]);
    putchar('\n');
    const unsigned char expected[] = {0xf8, 0xe6, 0x51, 0x32, 0xe0, 0x00};
    int matched = size == sizeof(expected) && memcmp(cookie, expected, sizeof(expected)) == 0;
    for (UInt32 i = 0; !matched && i + 2 < size; ++i) {
        if (cookie[i] != 5) continue;
        UInt32 length = 0, offset = i + 1;
        unsigned groups = 0;
        do {
            if (offset >= size || ++groups > 4) break;
            length = (length << 7) | (cookie[offset] & 127);
        } while (cookie[offset++] & 128);
        if (groups <= 4 && length == sizeof(expected) && offset + length <= size &&
            memcmp(cookie + offset, expected, sizeof(expected)) == 0) matched = 1;
    }
    if (!matched) {
        fputs("Native encoder did not produce the supported Apple screen-sharing configuration\n", stderr);
        return 6;
    }
    FILE *fixture = fopen(argv[1], "wb");
    FILE *config_file = fopen("audio-config.bin", "wb");
    if (!fixture || !config_file) return 7;
    if (fwrite(expected, 1, sizeof(expected), config_file) != sizeof(expected)) return 8;
    fclose(config_file);
    float pcm[480 * 2];
    unsigned char encoded[4096];
    unsigned packets_written = 0;
    for (unsigned block = 0; block < 160 && packets_written < 100; ++block) {
        for (unsigned i = 0; i < 480; ++i) {
            double time = (block * 480 + i) / 48000.0;
            pcm[2 * i] = (float)(0.1 * sin(6.283185307179586 * 440 * time));
            pcm[2 * i + 1] = (float)(0.1 * sin(6.283185307179586 * 880 * time));
        }
        UInt32 bytes = sizeof(pcm), packets = 480;
        CHECK(AudioCodecAppendInputData(instance, pcm, &bytes, &packets, NULL));
        if (bytes != sizeof(pcm) || packets != 480) return 9;
        for (unsigned drain = 0; drain < 8 && packets_written < 100; ++drain) {
            bytes = sizeof(encoded);
            packets = 1;
            UInt32 status = 0;
            AudioStreamPacketDescription packet = {0};
            CHECK(AudioCodecProduceOutputPackets(instance, encoded, &bytes, &packets, &packet, &status));
            if (!packets) break;
            if (packets != 1 || !bytes || bytes > sizeof(encoded) || packet.mStartOffset != 0 ||
                packet.mDataByteSize != bytes) return 10;
            if (fputc((int)(bytes >> 8), fixture) == EOF || fputc((int)(bytes & 255), fixture) == EOF ||
                fwrite(encoded, 1, bytes, fixture) != bytes) return 11;
            ++packets_written;
        }
    }
    fclose(fixture);
    CHECK(AudioCodecUninitialize(instance));
    CHECK(AudioComponentInstanceDispose(instance));
    printf("Original synthetic stereo 440/880Hz, 48kHz/480 frames, %u raw Apple AAC-ELD-SBR packets\n", packets_written);
    return packets_written == 100 ? 0 : 12;
}
