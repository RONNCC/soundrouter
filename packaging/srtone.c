// Silent-by-design test tone: plays a 1 kHz stereo sine into a VIRTUAL device's output only.
//   srtone <device-uid> [secs=10] [L=0.2] [R=0.1] [first-ch=0]
// Refuses (exit 2) any device that is not a soundrouter:* or BlackHole device, so it can never reach
// speakers or headphones. Being its own process, the engine can tap it with source `path:<this binary>`.
// first-ch puts L/R on output channels first-ch, first-ch+1 (other channels stay silent), so a test can
// keep the player off the channels it measures on a many-channel device such as BlackHole 16ch.
#include <CoreAudio/CoreAudio.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static float gAmp[2]; static UInt32 gFirst, gCh; static double gStep, gPhase;

static OSStatus io(AudioObjectID d, const AudioTimeStamp* now, const AudioBufferList* in, const AudioTimeStamp* inT, AudioBufferList* out, const AudioTimeStamp* outT, void* u) {
    for (UInt32 b = 0, base = 0; b < out->mNumberBuffers; b++) {
        AudioBuffer* ab = &out->mBuffers[b]; UInt32 nc = ab->mNumberChannels;
        float* o = ab->mData; if (!o || !nc) continue;
        UInt32 n = ab->mDataByteSize / sizeof(float) / nc;
        memset(o, 0, ab->mDataByteSize);
        double ph = gPhase;
        for (UInt32 f = 0; f < n; f++, ph += gStep) {
            float s = (float)sin(ph);
            for (UInt32 c = 0; c < 2; c++) if (gFirst + c >= base && gFirst + c < base + nc) o[f * nc + (gFirst + c - base)] = s * gAmp[c];
        }
        if (b + 1 == out->mNumberBuffers) gPhase = fmod(ph, 2 * M_PI);
        base += nc;
    }
    return 0;
}

static int getstr(AudioObjectID d, AudioObjectPropertySelector sel, char* buf, size_t n) {
    AudioObjectPropertyAddress a = { sel, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFStringRef s = NULL; UInt32 sz = sizeof s;
    if (AudioObjectGetPropertyData(d, &a, 0, NULL, &sz, &s) || !s) return 0;
    int ok = CFStringGetCString(s, buf, n, kCFStringEncodingUTF8); CFRelease(s); return ok;
}

int main(int argc, char** argv) {
    if (argc < 2) { fprintf(stderr, "usage: srtone <device-uid> [secs] [L] [R] [first-ch]\n"); return 1; }
    const char* uid = argv[1];
    double secs = argc > 2 ? atof(argv[2]) : 10;
    gAmp[0] = argc > 3 ? atof(argv[3]) : 0.2f; gAmp[1] = argc > 4 ? atof(argv[4]) : 0.1f;
    gFirst = argc > 5 ? (UInt32)atoi(argv[5]) : 0;
    if (secs <= 0 || secs > 3600 || gAmp[0] < 0 || gAmp[0] > 1 || gAmp[1] < 0 || gAmp[1] > 1) { fprintf(stderr, "srtone: bad secs/amplitude\n"); return 1; }

    AudioObjectPropertyAddress ta = { kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
    CFStringRef u = CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8); AudioObjectID d = 0; UInt32 sz = sizeof d;
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &ta, sizeof u, &u, &sz, &d);
    if (!d) { fprintf(stderr, "srtone: no device with uid %s\n", uid); return 2; }
    // Hard allowlist, checked on what the device reports (not on the argument): never a physical output.
    char devuid[256] = "", name[256] = "";
    getstr(d, kAudioDevicePropertyDeviceUID, devuid, sizeof devuid); getstr(d, kAudioObjectPropertyName, name, sizeof name);
    if (strncmp(devuid, "soundrouter:", 12) && !strstr(name, "BlackHole")) { fprintf(stderr, "srtone: refusing '%s' (%s): only soundrouter:* or BlackHole devices\n", name, devuid); return 2; }

    AudioStreamBasicDescription f; sz = sizeof f;
    AudioObjectPropertyAddress va = { kAudioDevicePropertyStreamFormat, kAudioObjectPropertyScopeOutput, 0 };
    if (AudioObjectGetPropertyData(d, &va, 0, NULL, &sz, &f) || !(f.mFormatFlags & kAudioFormatFlagIsFloat) || f.mChannelsPerFrame < gFirst + 2) {
        fprintf(stderr, "srtone: '%s' has no float output with channels %u-%u\n", name, gFirst, gFirst + 1); return 2;
    }
    gCh = f.mChannelsPerFrame; gStep = 2 * M_PI * 1000.0 / f.mSampleRate;
    AudioDeviceIOProcID pid; OSStatus e = AudioDeviceCreateIOProcID(d, io, NULL, &pid);
    if (!e) e = AudioDeviceStart(d, pid);
    if (e) { fprintf(stderr, "srtone: start failed (%d)\n", (int)e); return 1; }
    printf("srtone: %.1f s on '%s' (%s) ch %u-%u L=%.2f R=%.2f\n", secs, name, devuid, gFirst, gFirst + 1, gAmp[0], gAmp[1]); fflush(stdout);
    usleep((useconds_t)(secs * 1e6));
    AudioDeviceStop(d, pid); AudioDeviceDestroyIOProcID(d, pid);
    return 0;
}
