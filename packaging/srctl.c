// App-side contract test for the SoundRouter driver. Uses only the public client HAL API.
//   srctl get                         print current config plist
//   srctl set uid:name:ch [...]       replace device list (e.g. sr.v1:"Loopback 1":2)
//   srctl list                        UIDs of soundrouter: devices the system lists (hidden companions must NOT appear)
//   srctl vol|mute <uid> in|out [value]  read or set V's volume (scalar) / mute control
//   srctl vmv <uid> [scalar]          the virtual main volume (output scope) the system volume keys set
//   srctl srov <uid>                  the driver's late/short counters of V and its companion C
//   srctl loop <uid> [frames] [secs]  write impulses to the device output, read its input, report delay (min/mean/max) and the driver's srov growth
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

// Volume/mute control objects of a device scope (what Sound settings and the volume keys drive).
static AudioObjectID control(AudioObjectID d, UInt32 cls, UInt32 scope) {
    AudioObjectPropertyAddress ca = { kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    AudioObjectID ids[16]; UInt32 sz = sizeof ids;
    if (AudioObjectGetPropertyData(d, &ca, 0, NULL, &sz, ids)) return 0;
    for (UInt32 k = 0; k < sz / sizeof(AudioObjectID); k++) {
        AudioObjectPropertyAddress ka = { kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        AudioObjectPropertyAddress sa = { kAudioControlPropertyScope, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        UInt32 c = 0, s = 0, n = 4;
        AudioObjectGetPropertyData(ids[k], &ka, 0, NULL, &n, &c); n = 4;
        AudioObjectGetPropertyData(ids[k], &sa, 0, NULL, &n, &s);
        if (c == cls && s == scope) return ids[k];
    }
    return 0;
}

// Loopback probe: impulse every 4800 frames into output ch1; measure host-time delay until it shows on input ch1.
static volatile UInt64 gSentHost, gDelaySum, gDelayMin = ~0ULL, gDelayMax; static volatile int gLost; static UInt64 gLostAfter, gWarm; static volatile int gN, gArmed; static UInt32 gCh; // gWarm: host time before which echoes are not counted (read-ahead calibrates over the first cycles)
static OSStatus io(AudioObjectID d, const AudioTimeStamp* now, const AudioBufferList* in, const AudioTimeStamp* inT, AudioBufferList* out, const AudioTimeStamp* outT, void* u) {
    static UInt64 frames;
    float* o = out->mBuffers[0].mData; UInt32 n = out->mBuffers[0].mDataByteSize / 4 / gCh;
    memset(o, 0, out->mBuffers[0].mDataByteSize);
    const float* i = in->mBuffers[0].mData;
    for (UInt32 f = 0; f < n; f++) if (gArmed && i[f * gCh] > 0.5f) { UInt64 dt = mach_absolute_time() - gSentHost; gArmed = 0; if (mach_absolute_time() < gWarm) continue; gDelaySum += dt; if (dt < gDelayMin) gDelayMin = dt; if (dt > gDelayMax) gDelayMax = dt; gN++; }
    if (gArmed && mach_absolute_time() - gSentHost > gLostAfter) { gLost++; gArmed = 0; } // impulse never came back: count it, send the next
    if (!gArmed && frames % 4800 < n) { o[0] = 1.0f; gSentHost = mach_absolute_time(); gArmed = 1; }
    frames += n; return 0;
}

// The driver's late/short counter ('srov') of a device by uid; -1 when the device or property is missing.
static long long srov(const char* uid) {
    AudioObjectID d = device(uid); if (!d) return -1;
    AudioObjectPropertyAddress pa = { 'srov', kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFNumberRef n = NULL; UInt32 sz = sizeof n; long long v = -1;
    if (AudioObjectGetPropertyData(d, &pa, 0, NULL, &sz, &n) || !n) return -1;
    CFNumberGetValue(n, kCFNumberSInt64Type, &v); CFRelease(n); return v;
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
            // parse from the right: uid may itself contain ':' (soundrouter:v1)
            char uid[128], name[128], *end; long ch; const char* sp = argv[k];
            const char* c2 = strrchr(sp, ':'); const char* c1 = NULL;
            if (c2) for (const char* q = c2; q-- > sp;) if (*q == ':') { c1 = q; break; }
            if (!c1) { fprintf(stderr, "bad spec %s\n", sp); return 1; }
            ch = strtol(c2 + 1, &end, 10);
            if (end == c2 + 1 || *end || ch < 1 || ch > 16 || c1 == sp || (size_t)(c1 - sp) >= sizeof uid || (size_t)(c2 - c1 - 1) >= sizeof name) { fprintf(stderr, "bad spec %s\n", sp); return 1; }
            memcpy(uid, sp, c1 - sp); uid[c1 - sp] = 0;
            memcpy(name, c1 + 1, c2 - c1 - 1); name[c2 - c1 - 1] = 0;
            int chi = (int)ch;
            CFStringRef keys[3] = { CFSTR("uid"), CFSTR("name"), CFSTR("channels") };
            CFTypeRef vals[3] = { CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8), CFStringCreateWithCString(0, name, kCFStringEncodingUTF8), CFNumberCreate(0, kCFNumberIntType, &chi) };
            CFArrayAppendValue(arr, CFDictionaryCreate(0, (const void**)keys, vals, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks));
        }
        OSStatus e = AudioObjectSetPropertyData(plugin(), &a, 0, NULL, sizeof(CFPropertyListRef), &arr);
        printf("set -> %d\n", (int)e); return e != 0;
    }
    // srctl vol <uid> in|out [scalar]; srctl mute <uid> in|out [0|1]: the control objects (print the value, or set it).
    // srctl vmv <uid> [scalar]: the system's virtual main volume on the output scope, which is what the volume keys set.
    if ((!strcmp(argv[1], "vol") || !strcmp(argv[1], "mute")) && argc >= 4) {
        int vol = !strcmp(argv[1], "vol");
        AudioObjectID d = device(argv[2]); if (!d) { fprintf(stderr, "no device %s\n", argv[2]); return 2; }
        AudioObjectID c = control(d, vol ? kAudioVolumeControlClassID : kAudioMuteControlClassID, !strcmp(argv[3], "in") ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput);
        if (!c) { fprintf(stderr, "no %s control on %s (driver without controls?)\n", argv[1], argv[2]); return 3; }
        AudioObjectPropertyAddress pa = { vol ? kAudioLevelControlPropertyScalarValue : kAudioBooleanControlPropertyValue, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
        if (argc > 4) {
            OSStatus e;
            if (vol) { Float32 v = atof(argv[4]); e = AudioObjectSetPropertyData(c, &pa, 0, NULL, 4, &v); }
            else { UInt32 v = atoi(argv[4]); e = AudioObjectSetPropertyData(c, &pa, 0, NULL, 4, &v); }
            if (e) { printf("set err %d\n", (int)e); return 1; }
        }
        if (vol) {
            Float32 v = 0, db = 0; UInt32 n = 4; AudioObjectGetPropertyData(c, &pa, 0, NULL, &n, &v);
            AudioObjectPropertyAddress da = { kAudioLevelControlPropertyDecibelValue, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
            n = 4; AudioObjectGetPropertyData(c, &da, 0, NULL, &n, &db); printf("scalar %.4f  %.2f dB\n", v, db);
        } else { UInt32 v = 0, n = 4; AudioObjectGetPropertyData(c, &pa, 0, NULL, &n, &v); printf("mute %u\n", v); }
        return 0;
    }
    if (!strcmp(argv[1], "vmv") && argc >= 3) {
        AudioObjectID d = device(argv[2]); if (!d) { fprintf(stderr, "no device %s\n", argv[2]); return 2; }
        AudioObjectPropertyAddress pa = { 'vmvc', kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain };
        if (argc > 3) { Float32 v = atof(argv[3]); OSStatus e = AudioObjectSetPropertyData(d, &pa, 0, NULL, 4, &v); if (e) { printf("set err %d\n", (int)e); return 1; } }
        Float32 v = -1; UInt32 n = 4; OSStatus e = AudioObjectGetPropertyData(d, &pa, 0, NULL, &n, &v);
        printf("virtual main volume %.4f (err %d)\n", v, (int)e); return e != 0;
    }
    if (!strcmp(argv[1], "srov") && argc >= 3) {
        char comp[160]; snprintf(comp, sizeof comp, "%s.engine", argv[2]);
        printf("srov V %lld C %lld\n", srov(argv[2]), srov(comp)); return 0;
    }
    if (!strcmp(argv[1], "loop") && argc >= 3) {
        AudioObjectID d = device(argv[2]); if (!d) { fprintf(stderr, "no device %s\n", argv[2]); return 2; }
        UInt32 frames = argc > 3 ? atoi(argv[3]) : 512, sz = 4;
        unsigned secs = argc > 4 ? atoi(argv[4]) : 3;
        char comp[160]; snprintf(comp, sizeof comp, "%s.engine", argv[2]);
        long long v0 = srov(argv[2]), c0 = srov(comp);
        AudioObjectPropertyAddress fa = { kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, 0 };
        AudioObjectSetPropertyData(d, &fa, 0, NULL, 4, &frames);
        AudioStreamBasicDescription f; sz = sizeof f;
        Float64 rate = 0; { AudioObjectPropertyAddress ra = { kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 0 }; UInt32 rs = sizeof rate; AudioObjectGetPropertyData(d, &ra, 0, NULL, &rs, &rate); }
        AudioObjectPropertyAddress va = { kAudioDevicePropertyStreamFormat, kAudioObjectPropertyScopeOutput, 0 };
        AudioObjectGetPropertyData(d, &va, 0, NULL, &sz, &f); gCh = f.mChannelsPerFrame;
        { mach_timebase_info_data_t t; mach_timebase_info(&t); gLostAfter = 500000000ULL * t.denom / t.numer; gWarm = mach_absolute_time() + 4 * gLostAfter; } // 0.5 s, warm-up 2 s
        AudioDeviceIOProcID pid; AudioDeviceCreateIOProcID(d, io, NULL, &pid); AudioDeviceStart(d, pid);
        sleep(secs); AudioDeviceStop(d, pid); AudioDeviceDestroyIOProcID(d, pid);
        mach_timebase_info_data_t tb; mach_timebase_info(&tb);
        if (!gN) { printf("FAIL: no impulse came back\n"); return 1; }
        double ms = (double)tb.numer / tb.denom / 1e6; long long v1 = srov(argv[2]), c1 = srov(comp);
        printf("buffer=%u frames @%.0f Hz ch=%u impulses=%d mean loopback delay=%.2f ms (min %.2f max %.2f) lost %d, srov V +%lld C +%lld\n", frames, rate, gCh, gN, (double)gDelaySum / gN * ms, gDelayMin * ms, gDelayMax * ms, gLost, v1 - v0, c1 < 0 ? 0 : c1 - c0);
        return 0;
    }
    return 1;
}
