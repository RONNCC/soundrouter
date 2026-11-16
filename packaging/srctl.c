// App-side contract test for the SoundRouter driver. Uses only the public client HAL API.
//   srctl get                         print current config plist
//   srctl set uid:name:ch [...]       replace device list (e.g. sr.v1:"Loopback 1":2)
//   srctl list                        UIDs of soundrouter: devices the system lists (hidden companions must NOT appear)
//   srctl loop <uid> [frames]         write impulses to the device output, read its input, report delay
#include <CoreAudio/CoreAudio.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define BUNDLE CFSTR("io.github.ronncc.soundrouter.driver")
#define SEL 'srcf'  // PROP_DEVICES in crates/sr-core/src/driver_proto.rs

static AudioObjectID plugin(void) {
    AudioObjectPropertyAddress a = { kAudioHardwarePropertyTranslateBundleIDToPlugIn, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFStringRef b = BUNDLE; AudioObjectID id = kAudioObjectUnknown; UInt32 sz = sizeof id;
    OSStatus e = AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof b, &b, &sz, &id);
    if (e || id == kAudioObjectUnknown) { fprintf(stderr, "driver not loaded (err %d)\n", (int)e); _exit(2); }
    return id;
}
static AudioObjectID device(const char* uid) {
    AudioObjectPropertyAddress a = { kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFStringRef u = CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8); AudioObjectID id = 0; UInt32 sz = sizeof id;
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, sizeof u, &u, &sz, &id); return id;
}

// Loopback probe: impulse every 4800 frames into output ch1; measure host-time delay until it shows on input ch1.
static volatile UInt64 gSentHost, gDelaySum; static volatile int gN, gArmed; static UInt32 gCh;
static OSStatus io(AudioObjectID d, const AudioTimeStamp* now, const AudioBufferList* in, const AudioTimeStamp* inT, AudioBufferList* out, const AudioTimeStamp* outT, void* u) {
    static UInt64 frames;
    float* o = out->mBuffers[0].mData; UInt32 n = out->mBuffers[0].mDataByteSize / 4 / gCh;
    memset(o, 0, out->mBuffers[0].mDataByteSize);
    const float* i = in->mBuffers[0].mData;
    for (UInt32 f = 0; f < n; f++) if (gArmed && i[f * gCh] > 0.5f) { gDelaySum += mach_absolute_time() - gSentHost; gN++; gArmed = 0; }
    if (!gArmed && frames % 4800 < n) { o[0] = 1.0f; gSentHost = mach_absolute_time(); gArmed = 1; }
    frames += n; return 0;
}

int main(int argc, char** argv) {
    if (argc < 2) return 1;
    AudioObjectPropertyAddress a = { SEL, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    if (!strcmp(argv[1], "get")) {
        CFPropertyListRef p = NULL; UInt32 sz = sizeof p;
        OSStatus e = AudioObjectGetPropertyData(plugin(), &a, 0, NULL, &sz, &p);
        if (e) { fprintf(stderr, "get err %d\n", (int)e); return 1; }
        CFShow(p); CFRelease(p); return 0;
    }
    if (!strcmp(argv[1], "list")) {
        AudioObjectPropertyAddress la = { kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        UInt32 sz = 0; AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &la, 0, NULL, &sz);
        AudioObjectID ids[512]; if (sz > sizeof ids) sz = sizeof ids;
        AudioObjectGetPropertyData(kAudioObjectSystemObject, &la, 0, NULL, &sz, ids);
        AudioObjectPropertyAddress ua = { kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        for (UInt32 k = 0; k < sz / sizeof(AudioObjectID); k++) {
            CFStringRef u = NULL; UInt32 us = sizeof u; char b[256];
            if (AudioObjectGetPropertyData(ids[k], &ua, 0, NULL, &us, &u) || !u) continue;
            if (CFStringGetCString(u, b, sizeof b, kCFStringEncodingUTF8) && !strncmp(b, "soundrouter:", 12)) puts(b);
            CFRelease(u);
        }
        return 0;
    }
    if (!strcmp(argv[1], "set")) {
        CFMutableArrayRef arr = CFArrayCreateMutable(0, 0, &kCFTypeArrayCallBacks);
        for (int k = 2; k < argc; k++) {
            char uid[128], name[128]; int ch;
            if (sscanf(argv[k], "%127[^:]:%127[^:]:%d", uid, name, &ch) != 3) { fprintf(stderr, "bad spec %s\n", argv[k]); return 1; }
            CFStringRef keys[3] = { CFSTR("uid"), CFSTR("name"), CFSTR("channels") };
            CFTypeRef vals[3] = { CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8), CFStringCreateWithCString(0, name, kCFStringEncodingUTF8), CFNumberCreate(0, kCFNumberIntType, &ch) };
            CFArrayAppendValue(arr, CFDictionaryCreate(0, (const void**)keys, vals, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks));
        }
        OSStatus e = AudioObjectSetPropertyData(plugin(), &a, 0, NULL, sizeof(CFPropertyListRef), &arr);
        printf("set -> %d\n", (int)e); return e != 0;
    }
    if (!strcmp(argv[1], "loop") && argc >= 3) {
        AudioObjectID d = device(argv[2]); if (!d) { fprintf(stderr, "no device %s\n", argv[2]); return 2; }
        UInt32 frames = argc > 3 ? atoi(argv[3]) : 512, sz = 4;
        AudioObjectPropertyAddress fa = { kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, 0 };
        AudioObjectSetPropertyData(d, &fa, 0, NULL, 4, &frames);
        AudioStreamBasicDescription f; sz = sizeof f;
        AudioObjectPropertyAddress va = { kAudioDevicePropertyStreamFormat, kAudioObjectPropertyScopeOutput, 0 };
        AudioObjectGetPropertyData(d, &va, 0, NULL, &sz, &f); gCh = f.mChannelsPerFrame;
        AudioDeviceIOProcID pid; AudioDeviceCreateIOProcID(d, io, NULL, &pid); AudioDeviceStart(d, pid);
        sleep(3); AudioDeviceStop(d, pid); AudioDeviceDestroyIOProcID(d, pid);
        mach_timebase_info_data_t tb; mach_timebase_info(&tb);
        if (!gN) { printf("FAIL: no impulse came back\n"); return 1; }
        printf("buffer=%u frames ch=%u impulses=%d mean loopback delay=%.2f ms\n", frames, gCh, gN, (double)gDelaySum / gN * tb.numer / tb.denom / 1e6);
        return 0;
    }
    return 1;
}
