// Fake coreaudiod host: loads SoundRouter.driver via CFPlugIn like the HAL and drives the vtable. No sudo.
// usage: fakehost <path/to/SoundRouter.driver> [fuzz SEED [STEPS]]. Prints "ALL OK" (or "FUZZ OK") or the first failing
// assertion (exit 1).
#include <CoreAudio/AudioServerPlugIn.h>
#include <dlfcn.h>
#include <stdio.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <math.h>
#include <mach/mach_time.h>
#define CHECK(c) do { if (!(c)) { printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); exit(1); } } while (0)
#define MAXDEV 16 // sr_core::driver_proto::MAX_DEVICES
#define FR 256

static CFPropertyListRef gStore;
static volatile int gNotifies, gStoreWrites;
static AudioServerPlugInDriverRef gDrv;
#define D (*gDrv)

static struct { AudioObjectID o; UInt32 s; } gNL[256]; // every PropertiesChanged the driver sent since the last reset
static volatile int gNLn;
static volatile int gProbe, gGap; // 6c: while gProbe, a PLUGIN DeviceList notification runs gapProbe on the notifying (worker) thread
static volatile int gStorm; // 6d: likewise for stormProbe
static volatile int gFuzz; // 9: notifications are checked, not printed
static void gapProbe(void);
static void stormProbe(void);
static void fuzzNote(AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress* a);

static OSStatus PropsChanged(AudioServerPlugInHostRef h, AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress* a) {
    if (gFuzz) fuzzNote(o, n, a);
    for (UInt32 i = 0; i < n; i++) {
        UInt32 s = CFSwapInt32HostToBig(a[i].mSelector);
        if (!gFuzz) printf("  host.PropertiesChanged obj=%u sel='%.4s'\n", o, (char*)&s);
        int k = __atomic_fetch_add(&gNLn, 1, __ATOMIC_SEQ_CST);
        if (k < 256) { gNL[k].o = o; gNL[k].s = a[i].mSelector; }
        if (gProbe && o == kAudioObjectPlugInObject && a[i].mSelector == kAudioPlugInPropertyDeviceList) gapProbe();
        if (gStorm && o == kAudioObjectPlugInObject && a[i].mSelector == kAudioPlugInPropertyDeviceList) stormProbe();
    }
    __atomic_add_fetch(&gNotifies, 1, __ATOMIC_SEQ_CST);
    return 0;
}
static OSStatus CopyStore(AudioServerPlugInHostRef h, CFStringRef k, CFPropertyListRef* out) {
    *out = gStore ? CFRetain(gStore) : NULL;
    return 0;
}
static OSStatus WriteStore(AudioServerPlugInHostRef h, CFStringRef k, CFPropertyListRef v) {
    if (gStore) CFRelease(gStore);
    gStore = CFRetain(v);
    __atomic_add_fetch(&gStoreWrites, 1, __ATOMIC_SEQ_CST);
    return 0;
}
static OSStatus DelStore(AudioServerPlugInHostRef h, CFStringRef k) { return 0; }
static int gChanges, gDefer, gQn;
enum { QMAX = 512 };
static struct { AudioObjectID d; UInt64 a; } gQ[QMAX]; // requests held back while gDefer, to run them one device at a time
static pthread_mutex_t gQm = PTHREAD_MUTEX_INITIALIZER; // the worker queues while the main thread performs
static OSStatus ReqChange(AudioServerPlugInHostRef h, AudioObjectID d, UInt64 act, void* info) {
    if (gDefer) {
        pthread_mutex_lock(&gQm);
        if (gQn < QMAX) { gQ[gQn].d = d; gQ[gQn++].a = act; }
        pthread_mutex_unlock(&gQm);
        return 0;
    }
    printf("  host.RequestDeviceConfigurationChange dev=%u action=%llu -> Perform\n", d, act);
    gChanges++;
    return D->PerformDeviceConfigurationChange(gDrv, d, act, info); // real host stops IO first
}
// Perform held-back request i and drop it from the queue.
static void performQ(int i) {
    CHECK(i < gQn && D->PerformDeviceConfigurationChange(gDrv, gQ[i].d, gQ[i].a, NULL) == 0);
    pthread_mutex_lock(&gQm);
    memmove(&gQ[i], &gQ[i + 1], (size_t)(gQn - i - 1) * sizeof gQ[0]); gQn--;
    pthread_mutex_unlock(&gQm);
}
static AudioServerPlugInHostInterface gHost = { PropsChanged, CopyStore, WriteStore, DelStore, ReqChange };

static AudioObjectPropertyAddress addr(UInt32 sel, UInt32 scope) { return (AudioObjectPropertyAddress){ sel, scope, 0 }; }
static UInt32 u32(AudioObjectID o, UInt32 sel, UInt32 scope) {
    AudioObjectPropertyAddress a = addr(sel, scope); UInt32 v = 0, sz = 4;
    D->GetPropertyData(gDrv, o, 0, &a, 0, NULL, 4, &sz, &v);
    return v;
}
static UInt32 ids(AudioObjectID o, UInt32 sel, UInt32 scope, AudioObjectID* out) {
    AudioObjectPropertyAddress a = addr(sel, scope); UInt32 sz = 0;
    if (D->GetPropertyDataSize(gDrv, o, 0, &a, 0, NULL, &sz) || !sz) return 0;
    D->GetPropertyData(gDrv, o, 0, &a, 0, NULL, sz, &sz, out);
    return sz / 4;
}
static UInt32 nDevices(void) { AudioObjectID l[64]; return ids(kAudioObjectPlugInObject, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, l); }
static AudioObjectID byUID(const char* uid) {
    AudioObjectPropertyAddress a = addr(kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal);
    CFStringRef q = CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8); AudioObjectID id = 0; UInt32 sz = 4;
    D->GetPropertyData(gDrv, kAudioObjectPlugInObject, sizeof q, &a, sizeof q, &q, 4, &sz, &id);
    CFRelease(q);
    return id;
}
static AudioObjectID stream(AudioObjectID dev, UInt32 scope) { AudioObjectID s[4] = {0}; return ids(dev, kAudioDevicePropertyStreams, scope, s) == 1 ? s[0] : 0; }
static UInt32 chans(AudioObjectID st) {
    AudioObjectPropertyAddress a = addr(kAudioStreamPropertyVirtualFormat, 0); AudioStreamBasicDescription f; UInt32 sz = sizeof f;
    D->GetPropertyData(gDrv, st, 0, &a, 0, NULL, sz, &sz, &f);
    return f.mChannelsPerFrame;
}
static CFDictionaryRef dev(const char* uid, const char* name, int ch) {
    CFStringRef k[3] = { CFSTR("uid"), CFSTR("name"), CFSTR("channels") };
    CFTypeRef v[3] = { CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8), CFStringCreateWithCString(0, name, kCFStringEncodingUTF8), CFNumberCreate(0, kCFNumberIntType, &ch) };
    return CFDictionaryCreate(0, (const void**)k, v, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
}
// Set 'srcf'; on success wait for the async apply (worker thread) to notify.
static OSStatus setConfig(CFArrayRef arr) {
    AudioObjectPropertyAddress a = addr('srcf', kAudioObjectPropertyScopeGlobal);
    int before = gNotifies;
    OSStatus e = D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &a, 0, NULL, sizeof arr, &arr);
    if (e == 0) {
        for (int i = 0; i < 400 && gNotifies == before; i++) usleep(5000);
        for (int i = 0; i < 400 && nDevices() != 2 * CFArrayGetCount(arr); i++) usleep(10000); // a second pass may place the rest
        usleep(30000);
    }
    return e;
}
static CFArrayRef cfg(int n, const char* const* uid, const char* const* name, const int* ch) {
    CFMutableArrayRef a = CFArrayCreateMutable(0, n, &kCFTypeArrayCallBacks);
    for (int i = 0; i < n; i++) CFArrayAppendValue(a, dev(uid[i], name[i], ch[i]));
    return a;
}
static int hidden(AudioObjectID d) { return u32(d, 'hidn', kAudioObjectPropertyScopeGlobal); }
static int notified(AudioObjectID o, UInt32 sel) {
    int n = gNLn < 256 ? gNLn : 256;
    for (int i = 0; i < n; i++) if (gNL[i].o == o && gNL[i].s == sel) return 1;
    return 0;
}
static int nameIs(AudioObjectID d, const char* s) {
    AudioObjectPropertyAddress a = addr(kAudioObjectPropertyName, 0); CFStringRef n = NULL; UInt32 sz = sizeof n;
    if (D->GetPropertyData(gDrv, d, 0, &a, 0, NULL, sz, &sz, &n) || !n) return 0;
    CFStringRef w = CFStringCreateWithCString(0, s, kCFStringEncodingUTF8); int r = CFEqual(n, w);
    CFRelease(w); CFRelease(n);
    return r;
}
static Float64 rateOf(AudioObjectID d) {
    AudioObjectPropertyAddress a = addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal); Float64 r = 0; UInt32 sz = 8;
    D->GetPropertyData(gDrv, d, 0, &a, 0, NULL, 8, &sz, &r);
    return r;
}
static OSStatus setRate(AudioObjectID d, Float64 r, UInt32 size) {
    AudioObjectPropertyAddress a = addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal);
    return D->SetPropertyData(gDrv, d, 0, &a, 0, NULL, size, &r);
}
static AudioStreamBasicDescription fmtOf(AudioObjectID st) {
    AudioObjectPropertyAddress a = addr(kAudioStreamPropertyVirtualFormat, 0); AudioStreamBasicDescription f; UInt32 sz = sizeof f;
    D->GetPropertyData(gDrv, st, 0, &a, 0, NULL, sz, &sz, &f);
    return f;
}
static OSStatus setFmt(AudioObjectID st, UInt32 sel, AudioStreamBasicDescription f) {
    AudioObjectPropertyAddress a = addr(sel, 0);
    return D->SetPropertyData(gDrv, st, 0, &a, 0, NULL, sizeof f, &f);
}
// The live 'srcf' value (+1).
static CFArrayRef srcf(void) {
    AudioObjectPropertyAddress a = addr('srcf', kAudioObjectPropertyScopeGlobal); CFArrayRef l = NULL; UInt32 sz = sizeof l;
    CHECK(D->GetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &a, 0, NULL, sz, &sz, &l) == 0 && l);
    return l;
}
static int entryIs(CFArrayRef l, int i, const char* uid, const char* name, int ch) {
    CFDictionaryRef d = CFArrayGetValueAtIndex(l, i); int c = 0; if (!d) return 0;
    CFStringRef u = CFDictionaryGetValue(d, CFSTR("uid")), n = CFDictionaryGetValue(d, CFSTR("name"));
    CFNumberGetValue(CFDictionaryGetValue(d, CFSTR("channels")), kCFNumberIntType, &c);
    CFStringRef wu = CFStringCreateWithCString(0, uid, kCFStringEncodingUTF8), wn = CFStringCreateWithCString(0, name, kCFStringEncodingUTF8);
    int r = u && n && CFEqual(u, wu) && CFEqual(n, wn) && c == ch;
    CFRelease(wu); CFRelease(wn);
    return r;
}
// Simulate a coreaudiod restart: host storage holds `store` (NULL = nothing), then Initialize runs again.
static void reinit(CFPropertyListRef store) {
    if (gStore) CFRelease(gStore);
    gStore = store ? CFRetain(store) : NULL;
    CHECK(D->Initialize(gDrv, &gHost) == 0);
}

static AudioServerPlugInIOCycleInfo gCi;
static OSStatus io(AudioObjectID d, AudioObjectID st, UInt32 client, int op, double t, float* buf) {
    gCi.mInputTime.mSampleTime = gCi.mOutputTime.mSampleTime = t;
    return D->DoIOOperation(gDrv, d, st, client, op, FR, &gCi, buf, NULL);
}

// Level/mute controls and the overload counter (props on V's control objects).
static OSStatus getBytes(AudioObjectID o, UInt32 sel, UInt32 scope, void* out, UInt32 size) {
    AudioObjectPropertyAddress a = addr(sel, scope); UInt32 sz = size;
    return D->GetPropertyData(gDrv, o, 0, &a, 0, NULL, size, &sz, out);
}
static Float32 f32(AudioObjectID o, UInt32 sel) { Float32 v = -999; CHECK(getBytes(o, sel, 0, &v, 4) == 0); return v; }
static OSStatus setBytes(AudioObjectID o, UInt32 sel, UInt32 size, const void* v) {
    AudioObjectPropertyAddress a = addr(sel, 0);
    return D->SetPropertyData(gDrv, o, 0, &a, 0, NULL, size, v);
}
static void setF32(AudioObjectID o, UInt32 sel, Float32 v) { CHECK(setBytes(o, sel, 4, &v) == 0); }
static void setMute(AudioObjectID o, UInt32 m) { CHECK(setBytes(o, kAudioBooleanControlPropertyValue, 4, &m) == 0); }
static Float32 convert(AudioObjectID o, UInt32 sel, Float32 in) { Float32 v = in; CHECK(getBytes(o, sel, 0, &v, 4) == 0); return v; }
static SInt64 overloads(AudioObjectID d) {
    AudioObjectPropertyAddress a = addr('srov', kAudioObjectPropertyScopeGlobal); CFNumberRef n = NULL; UInt32 sz = sizeof n; SInt64 v = -1;
    CHECK(D->GetPropertyData(gDrv, d, 0, &a, 0, NULL, sz, &sz, &n) == 0 && n && CFGetTypeID(n) == CFNumberGetTypeID());
    CFNumberGetValue(n, kCFNumberSInt64Type, &v); CFRelease(n);
    return v;
}
// What the driver should multiply by for a dB value: the same libm pow, rounded to float once.
static Float32 gainOf(Float32 db) { return db <= -64.0f ? 0.0f : (Float32)pow(10.0, (double)db / 20.0); }
static void fill(float* b, int ch, float base) { for (int i = 0; i < FR * ch; i++) b[i] = base + (float)i; }

// 6c: 2-channel ReadInput on the ids of a removed slot (as a HAL that has not torn them down yet would do).
// Returns 1 when the guard words behind the 2-channel region stayed untouched.
enum { GUARD = 4096 };
_Static_assert(FR * 8 <= FR * 2 + GUARD, "a legal 8-channel read must fit the buffer");
static AudioObjectID gStaleDev[2], gStaleSt[2];
static int staleReadClean(int k) {
    float cb[FR * 2 + GUARD];
    for (int j = 0; j < FR * 2 + GUARD; j++) cb[j] = -777.0f;
    io(gStaleDev[k], gStaleSt[k], 7, kAudioServerPlugInIOOperationReadInput, 3000, cb);
    for (int j = FR * 2; j < FR * 2 + GUARD; j++) if (cb[j] != -777.0f) return 0;
    return 1;
}
static void gapProbe(void) {
    // The worker's first pass just announced the removal and has not run its second pass: the slot is dead,
    // so the stale ids must be refused and silent. Deterministic: no polling, no dependence on scheduling.
    if (byUID("soundrouter:sw15") || byUID("soundrouter:swide")) return;
    CHECK(staleReadClean(0) && staleReadClean(1));
    gGap = 1;
}
// 6d: every DeviceList notification during the storm. The storm started at gStormT0, so any slot freed by it was freed
// after that: until 1 s (the driver's TEARDOWN) later, the stale 2-channel ids of slot 15 must not read as a wider device.
// One-sided in time: scheduling delays only make the re-creation later, never earlier. All on one reconcile thread.
static uint64_t gStormT0; static double gTps;
static pthread_t gStormThread; static volatile int gStormNotes, gStormThreads;
static void stormProbe(void) {
    pthread_t me = pthread_self();
    if (!gStormNotes++) gStormThread = me;
    if (!pthread_equal(me, gStormThread)) gStormThreads = 2;
    for (int k = 0; k < 2; k++) {
        if (staleReadClean(k)) continue;
        double since = (double)(mach_absolute_time() - gStormT0) / gTps;
        printf("  storm: slot 15 re-created %.3f s after the storm started\n", since);
        CHECK(since >= 1.0);
    }
}

// 9. Randomized scenario fuzz: fakehost <driver> fuzz SEED [STEPS]. The seed fixes the sequence of host actions; the
// reconcile worker's timing varies, so every check is one-sided in time (it can miss a bug, never flake).
// The host model re-reads the device list before each step, like a HAL that handles notifications a little late, and
// keeps doing I/O on a removed device's ids for up to 0.8 s after it last saw it. I/O buffers are sized by the format the
// host last read, with guard words behind. Configuration changes are queued and performed or aborted by this thread in
// random order, so I/O on a device is always stopped while its change runs. Lists (valid, invalid, repeated), rates,
// volumes and I/O start/stop are random; a second thread reads random properties of random objects throughout.
static unsigned long long gSeed; static volatile int gStep; static uint64_t gRng;
static uint32_t rnd(uint32_t n) { gRng ^= gRng << 13; gRng ^= gRng >> 7; gRng ^= gRng << 17; return (uint32_t)(gRng % n); }
static double since(uint64_t t) { return (double)(mach_absolute_time() - t) / gTps; }
#define FCHECK(c) do { if (!(c)) { printf("FAIL seed %llu step %d %s:%d %s\n", gSeed, gStep, __FILE__, __LINE__, #c); exit(1); } } while (0)

typedef struct { char uid[160], name[256]; int ch; } LDev;
static LDev gE[MAXDEV]; static int gEn; // the list srcf must return (names as the driver keeps them: trimmed, 64 chars)

// What the host knows about one device id. `seen`: time just before the last list read that showed it alive.
typedef struct { AudioObjectID id; char uid[160]; int engine, live, known, started; UInt32 ch; uint64_t seen; double t; Float64 rate; Float32 vol[2]; UInt32 mute[2]; } HDev;
enum { HMAX = 4096, GW = 1024 };
static HDev gH[HMAX]; static int gHn;
#define SENT 0x7FC0DEADu // guard word: a NaN the driver never produces

static int uidOf(AudioObjectID d, char* out, size_t n) {
    AudioObjectPropertyAddress a = addr(kAudioDevicePropertyDeviceUID, 0); CFStringRef s = NULL; UInt32 sz = sizeof s;
    if (D->GetPropertyData(gDrv, d, 0, &a, 0, NULL, sz, &sz, &s) || !s) return 0;
    int ok = CFStringGetCString(s, out, (CFIndex)n, kCFStringEncodingUTF8); CFRelease(s);
    return ok;
}
static UInt32 chansOf(AudioObjectID d, UInt32 scope) { // 0 when the device is gone
    AudioObjectID st[4]; if (ids(d, kAudioDevicePropertyStreams, scope, st) != 1) return 0;
    AudioObjectPropertyAddress a = addr(kAudioStreamPropertyVirtualFormat, 0); AudioStreamBasicDescription f; UInt32 sz = sizeof f;
    return D->GetPropertyData(gDrv, st[0], 0, &a, 0, NULL, sz, &sz, &f) ? 0 : f.mChannelsPerFrame;
}
static int sameDev(const HDev* e) { char u[160]; return uidOf(e->id, u, sizeof u) && !strcmp(u, e->uid) && since(e->seen) < 1.0; }
// A check on a device the host believes alive. A failure only counts while that device still exists and was seen less than
// 1 s ago (the driver may legitimately re-create a slot 1 s after a removal the host did not observe).
#define LCHECK(e, c) do { if (!(c) && sameDev(e)) { printf("FAIL seed %llu step %d %s:%d %s (%s id %u)\n", gSeed, gStep, __FILE__, __LINE__, #c, (e)->uid, (e)->id); exit(1); } } while (0)

static void fuzzNote(AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress* a) {
    if (o == kAudioObjectPlugInObject) { // one notification per list change: owned objects, then the device list
        FCHECK(n == 2 && a[0].mSelector == kAudioObjectPropertyOwnedObjects && a[1].mSelector == kAudioPlugInPropertyDeviceList);
        return;
    }
    for (UInt32 i = 0; i < n; i++) {
        UInt32 s = a[i].mSelector;
        if (s == kAudioObjectPropertyName) FCHECK(u32(o, kAudioObjectPropertyClass, 0) == kAudioDeviceClassID); // from a pass: device alive
        else FCHECK(s == kAudioLevelControlPropertyScalarValue || s == kAudioLevelControlPropertyDecibelValue || s == kAudioBooleanControlPropertyValue);
    }
}

static void poolUid(int i, char* out) { // 24 uids; the last is the longest valid one (128 bytes)
    if (i < 23) { snprintf(out, 160, "soundrouter:f%d", i); return; }
    strcpy(out, "soundrouter:"); size_t l = strlen(out); memset(out + l, 'L', 128 - l); out[128] = 0;
}
static int inList(const LDev* l, int n, const char* uid) { for (int i = 0; i < n; i++) if (!strcmp(l[i].uid, uid)) return 1; return 0; }
static void newUid(const LDev* l, int n, char* out) { do poolUid(rnd(24), out); while (inList(l, n, out)); }
static void rndName(char* out) {
    switch (rnd(6)) {
    case 0: snprintf(out, 256, "  Spaced %u\t", rnd(100)); break; // trimmed by the driver
    case 1: memset(out, 'N', 70); out[70] = 0; out[0] = (char)('A' + rnd(26)); break; // cut to 64
    case 2: snprintf(out, 256, "\xC3\x9Cn\xC3\xAF \xE2\x9C\x93 %u", rnd(100)); break; // "Ünï ✓ n"
    default: snprintf(out, 256, "Dev %u", rnd(1000)); break;
    }
}
static int rndCh(void) { static const int c[] = { 1, 2, 2, 2, 3, 4, 6, 8, 16, 64 }; return c[rnd(10)]; }
static void canon(const char* raw, char* out) { // Rust str::trim (only ASCII whitespace is generated), then 64 chars
    while (*raw == ' ' || *raw == '\t') raw++;
    size_t n = strlen(raw), i = 0; int chars = 0;
    while (n && (raw[n - 1] == ' ' || raw[n - 1] == '\t')) n--;
    for (; i < n; i++) if ((raw[i] & 0xC0) != 0x80 && chars++ == 64) break;
    memcpy(out, raw, i); out[i] = 0;
}
static CFStringRef cfs(const char* s) { return CFStringCreateWithCString(0, s, kCFStringEncodingUTF8); }
static CFDictionaryRef mkDict(CFTypeRef uid, CFTypeRef name, CFTypeRef ch) { // NULL leaves the key out
    CFStringRef k[3] = { CFSTR("uid"), CFSTR("name"), CFSTR("channels") }; CFTypeRef v[3] = { uid, name, ch };
    const void* ks[3]; const void* vs[3]; int n = 0;
    for (int i = 0; i < 3; i++) if (v[i]) { ks[n] = k[i]; vs[n++] = v[i]; }
    return CFDictionaryCreate(0, ks, vs, n, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
}
static CFMutableArrayRef mkList(const LDev* l, int n) {
    CFMutableArrayRef a = CFArrayCreateMutable(0, n, &kCFTypeArrayCallBacks);
    for (int i = 0; i < n; i++) {
        CFStringRef u = cfs(l[i].uid), m = cfs(l[i].name); CFNumberRef c = CFNumberCreate(0, kCFNumberIntType, &l[i].ch);
        CFDictionaryRef d = mkDict(u, m, c); CFArrayAppendValue(a, d);
        CFRelease(d); CFRelease(u); CFRelease(m); CFRelease(c);
    }
    return a;
}
static OSStatus setList(CFTypeRef v, UInt32 size) {
    AudioObjectPropertyAddress a = addr('srcf', kAudioObjectPropertyScopeGlobal);
    return D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &a, 0, NULL, size, &v);
}
static int listIs(CFTypeRef l) { // l (srcf or the store) holds exactly gE
    if (!l || CFGetTypeID(l) != CFArrayGetTypeID() || CFArrayGetCount(l) != gEn) return 0;
    for (int i = 0; i < gEn; i++) if (!entryIs(l, i, gE[i].uid, gE[i].name, gE[i].ch)) return 0;
    return 1;
}
static void checkSrcf(void) { CFArrayRef l = srcf(); FCHECK(listIs(l)); CFRelease(l); FCHECK(gStoreWrites ? listIs(gStore) : !gStore); }

// A device the host just found (fresh), or found again after not seeing it for 1 s (it may be a re-created one: unknown).
static void reload(HDev* e, uint64_t seen, int fresh) {
    e->live = 1; e->seen = seen; e->t = 0; e->known = fresh; e->started = fresh ? 0 : -1; e->rate = 0;
    e->ch = chansOf(e->id, kAudioObjectPropertyScopeOutput);
    for (int k = 0; k < 2; k++) { e->vol[k] = 1; e->mute[k] = 0; }
    if (!fresh) return;
    // A new device: matching formats, the default rate, idle, and (V) controls at their defaults, whatever ran in this slot before.
    LCHECK(e, e->ch >= 1 && e->ch <= 64 && chansOf(e->id, kAudioObjectPropertyScopeInput) == e->ch);
    LCHECK(e, hidden(e->id) == e->engine && rateOf(e->id) == 48000 && u32(e->id, kAudioDevicePropertyDeviceIsRunning, 0) == 0);
    if (e->engine) return;
    AudioObjectID c[8]; Float32 v0 = -1, v1 = -1;
    LCHECK(e, ids(e->id, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, c) == 4);
    getBytes(c[0], kAudioLevelControlPropertyScalarValue, 0, &v0, 4); getBytes(c[2], kAudioLevelControlPropertyScalarValue, 0, &v1, 4);
    LCHECK(e, v0 == 1 && v1 == 1 && u32(c[1], kAudioBooleanControlPropertyValue, 0) == 0 && u32(c[3], kAudioBooleanControlPropertyValue, 0) == 0);
}
static void hpoll(void) {
    int m = 0;
    for (int j = 0; j < gHn; j++) if (gH[j].live || since(gH[j].seen) < 2.0) gH[m++] = gH[j];
    gHn = m;
    uint64_t t0 = mach_absolute_time();
    // One call with room for every id: a size read first could be stale and truncate the list.
    AudioObjectID l[64]; UInt32 n = sizeof l; AudioObjectPropertyAddress la = addr(kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal);
    FCHECK(D->GetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &la, 0, NULL, n, &n, l) == 0);
    n /= sizeof *l;
    FCHECK(n <= 2 * MAXDEV && n % 2 == 0); // V and C come and go together
    static int found[HMAX]; memset(found, 0, sizeof found);
    for (UInt32 k = 0; k < n; k++) {
        char u[160]; if (!uidOf(l[k], u, sizeof u)) continue; // removed since the list was read
        uint64_t t1 = mach_absolute_time();
        HDev* e = NULL;
        for (int j = 0; j < gHn; j++) if (gH[j].live && gH[j].id == l[k]) e = &gH[j];
        if (e && !strcmp(e->uid, u)) {
            if ((double)(t1 - e->seen) / gTps >= 1.0) reload(e, t0, 0); else e->seen = t0;
            found[e - gH] = 1;
            continue;
        }
        // A new device on this id: whatever the host saw there before was last seen >= 1 s ago (the teardown quarantine).
        for (int j = 0; j < gHn; j++) if (gH[j].id == l[k]) {
            double ago = (double)(t1 - gH[j].seen) / gTps;
            if (ago < 1.0) printf("  id %u: %s (live %d) last seen %.3f s ago, now %s\n", l[k], gH[j].uid, gH[j].live, ago, u);
            FCHECK(ago >= 1.0);
        }
        if (e) e->live = 0;
        FCHECK(gHn < HMAX);
        e = &gH[gHn]; memset(e, 0, sizeof *e); found[gHn++] = 1;
        e->id = l[k]; strcpy(e->uid, u);
        size_t ul = strlen(u); e->engine = ul > 7 && !strcmp(u + ul - 7, ".engine");
        reload(e, t0, 1);
    }
    for (int j = 0; j < gHn; j++) if (!found[j]) gH[j].live = 0;
}
static HDev* randomLive(int visibleOnly) {
    int c[HMAX], n = 0;
    for (int j = 0; j < gHn; j++) if (gH[j].live && gH[j].ch && !(visibleOnly && gH[j].engine)) c[n++] = j;
    return n ? &gH[c[rnd(n)]] : NULL;
}
static HDev* partner(const HDev* e) {
    char p[200]; size_t l = strlen(e->uid);
    if (e->engine) { memcpy(p, e->uid, l - 7); p[l - 7] = 0; } else snprintf(p, sizeof p, "%s.engine", e->uid);
    for (int j = 0; j < gHn; j++) if (gH[j].live && !strcmp(gH[j].uid, p)) return &gH[j];
    return NULL;
}

static void stepList(void) {
    LDev c[MAXDEV]; int n = gEn; memcpy(c, gE, sizeof c);
    int kind = rnd(10), reps = 1 + (int)rnd(3);
    if (kind == 8) { n = (int)rnd(MAXDEV + 1); for (int i = 0; i < n; i++) { newUid(c, i, c[i].uid); rndName(c[i].name); c[i].ch = rndCh(); } }
    else if (kind == 9) for (; n < MAXDEV; n++) { newUid(c, n, c[n].uid); rndName(c[n].name); c[n].ch = rndCh(); } // a full pool
    else if (kind != 7) for (int r = 0; r < reps; r++) { // kind 7: the same list again
        int i = n ? (int)rnd(n) : 0;
        switch (rnd(6)) {
        case 0: if (n < MAXDEV) { newUid(c, n, c[n].uid); rndName(c[n].name); c[n].ch = rndCh(); n++; } break;
        case 1: if (n) { memmove(&c[i], &c[i + 1], (size_t)(n - i - 1) * sizeof c[0]); n--; } break;
        case 2: if (n) rndName(c[i].name); break;
        case 3: if (n) c[i].ch = rndCh(); break;
        case 4: if (n > 1) { LDev t = c[i]; int j = (i + 1) % n; c[i] = c[j]; c[j] = t; } break;
        case 5: if (n) { char u[160]; newUid(c, n, u); strcpy(c[i].uid, u); c[i].ch = rndCh(); } break; // swap a device for another one
        }
    }
    if (rnd(20) == 0) { CFArrayRef a = mkList(c, n); FCHECK(setList(a, 4) != 0); CFRelease(a); checkSrcf(); } // wrong size
    CFArrayRef a = mkList(c, n);
    for (int i = 0; i < n; i++) { char t[256]; canon(c[i].name, t); strcpy(c[i].name, t); }
    int same = n == gEn, w = gStoreWrites;
    for (int i = 0; same && i < n; i++) same = !strcmp(c[i].uid, gE[i].uid) && !strcmp(c[i].name, gE[i].name) && c[i].ch == gE[i].ch;
    FCHECK(setList(a, sizeof a) == 0);
    CFRelease(a);
    memcpy(gE, c, sizeof gE); gEn = n;
    FCHECK(gStoreWrites == w + !same); // persisted synchronously; the same list is not written again
    checkSrcf();
}
static void stepBadList(void) {
    LDev c[MAXDEV + 1]; int n = gEn; memcpy(c, gE, sizeof gE);
    if (n < 2) for (; n < 2; n++) { newUid(c, n, c[n].uid); strcpy(c[n].name, "X"); c[n].ch = 2; }
    int i = (int)rnd(n), kind = (int)rnd(19);
    CFTypeRef bad = NULL;
    switch (kind) {
    case 0: for (; n <= MAXDEV; n++) { snprintf(c[n].uid, 160, "soundrouter:x%d", n); strcpy(c[n].name, "X"); c[n].ch = 2; } break; // 17 devices
    case 1: strcpy(c[(i + 1) % n].uid, c[i].uid); break; // duplicate uid
    case 2: c[i].ch = rnd(2) ? 0 : 65; break;
    case 3: c[i].ch = rnd(2) ? -2 : 70000; break;
    case 4: strcpy(c[i].uid, rnd(2) ? "other:x" : "soundrouter:"); break; // foreign namespace, bare prefix
    case 5: snprintf(c[i].uid, 160, "soundrouter:f%u.engine", rnd(24)); break; // a companion uid
    case 6: strcpy(c[i].uid, "soundrouter:"); memset(c[i].uid + 12, 'L', 117); c[i].uid[129] = 0; break; // 129 bytes
    case 7: strcpy(c[i].name, rnd(2) ? "" : " \t "); break;
    case 8: c[i].uid[0] = 0; break;
    }
    if (kind <= 8) bad = mkList(c, n);
    else if (kind == 16) bad = CFStringCreateCopy(0, CFSTR("not a list"));
    else if (kind == 17) bad = NULL;
    else {
        CFMutableArrayRef a = mkList(c, n);
        CFStringRef u = cfs(c[i].uid), m = cfs(c[i].name); int ch = c[i].ch; double half = 2.5;
        CFNumberRef num = CFNumberCreate(0, kCFNumberIntType, &ch), dbl = CFNumberCreate(0, kCFNumberDoubleType, &half);
        UInt8 z[] = "soundrouter:a\0b"; CFStringRef nul = CFStringCreateWithBytes(0, z, 15, kCFStringEncodingUTF8, false);
        CFTypeRef d = NULL;
        switch (kind) {
        case 9: d = mkDict(u, num, num); break;          // name is a number
        case 10: d = mkDict(u, m, CFSTR("2")); break;    // channels is a string
        case 11: d = mkDict(u, m, dbl); break;           // channels 2.5
        case 12: d = mkDict(NULL, m, num); break;        // no uid
        case 13: d = mkDict(u, m, NULL); break;          // no channels
        case 14: d = mkDict(nul, m, num); break;         // NUL inside the uid
        case 15: d = CFRetain(CFSTR("not a dictionary")); break;
        case 18: d = mkDict(u, CFSTR(""), num); break;
        }
        CFArraySetValueAtIndex(a, i, d);
        CFRelease(d); CFRelease(u); CFRelease(m); CFRelease(num); CFRelease(dbl); CFRelease(nul);
        bad = a;
    }
    int w = gStoreWrites;
    FCHECK(setList(bad, sizeof bad) != 0);
    if (bad) CFRelease(bad);
    FCHECK(gStoreWrites == w);
    checkSrcf();
}
static void stepIO(void) {
    static int c[HMAX]; int nc = 0;
    for (int j = 0; j < gHn; j++) if (gH[j].ch && (gH[j].live || since(gH[j].seen) < 0.8)) c[nc++] = j; // removed: I/O for a while yet
    if (!nc) return;
    HDev* e = &gH[c[rnd(nc)]];
    // A live device's format only moves through a Perform this host ran (and then re-read).
    if (e->live) LCHECK(e, chansOf(e->id, kAudioObjectPropertyScopeOutput) == e->ch && chansOf(e->id, kAudioObjectPropertyScopeInput) == e->ch);
    UInt32 fr = rnd(20) == 0 ? 8100 + rnd(300) : rnd(4) == 0 ? 512 + rnd(3585) : 1 + rnd(512);
    static const UInt32 ops[] = { kAudioServerPlugInIOOperationReadInput, kAudioServerPlugInIOOperationWriteMix, kAudioServerPlugInIOOperationReadInput,
                                  kAudioServerPlugInIOOperationWriteMix, kAudioServerPlugInIOOperationReadInput, kAudioServerPlugInIOOperationWriteMix,
                                  kAudioServerPlugInIOOperationProcessOutput, kAudioServerPlugInIOOperationMixOutput,
                                  kAudioServerPlugInIOOperationConvertInput, kAudioServerPlugInIOOperationCycle };
    UInt32 op = ops[rnd(10)]; int wr = op == kAudioServerPlugInIOOperationWriteMix, rd = op == kAudioServerPlugInIOOperationReadInput;
    size_t n = (size_t)fr * e->ch;
    UInt32* b = malloc((n + GW) * sizeof *b); FCHECK(b);
    for (size_t i = 0; i < n; i++) { float x = (float)(i % 1000) - 500.0f; if (wr) memcpy(&b[i], &x, 4); else b[i] = SENT; }
    for (size_t i = n; i < n + GW; i++) b[i] = SENT;
    AudioServerPlugInIOCycleInfo ci; memset(&ci, 0, sizeof ci);
    ci.mInputTime.mSampleTime = ci.mOutputTime.mSampleTime = e->t;
    ci.mCurrentTime.mSampleTime = e->t + (rnd(8) ? -512.0 : 512.0); // sometimes late
    OSStatus st = D->DoIOOperation(gDrv, e->id, 0, 1, op, fr, &ci, b, NULL);
    double s = since(e->seen);
    int dirty = 0;
    for (size_t i = n; i < n + GW; i++) dirty |= b[i] != SENT;
    for (size_t i = 0; i < n && !rd; i++) { float x = (float)(i % 1000) - 500.0f; UInt32 w; memcpy(&w, &x, 4); dirty |= b[i] != (wr ? w : SENT); }
    if (dirty && s < 1.0) printf("  %s id %u op %u: %u frames x %u ch, seen %.3f s ago, live %d\n", e->uid, e->id, op, fr, e->ch, s, e->live);
    FCHECK(!(dirty && s < 1.0)); // nothing past the host's buffer, nothing in a buffer the op does not own
    // A removed device's ids answer nothing until the slot is reused, which is >= 1 s after the host last saw it.
    if (!e->live) FCHECK(s >= 1.0 || st == kAudioHardwareBadObjectError);
    else LCHECK(e, st == 0);
    e->t += fr + rnd(64);
    free(b);
}
static void stepStartStop(void) {
    HDev* e = randomLive(0); if (!e) return;
    OSStatus st;
    if (e->started > 0 && rnd(2)) { st = D->StopIO(gDrv, e->id, 1); if (!st) e->started--; }
    else { st = D->StartIO(gDrv, e->id, 1); if (!st && e->started >= 0) e->started++; }
    LCHECK(e, st == 0);
    if (e->started >= 0) LCHECK(e, u32(e->id, kAudioDevicePropertyDeviceIsRunning, 0) == (e->started > 0));
}
static void stepRate(void) {
    HDev* e = randomLive(0); if (!e) return;
    static const Float64 rates[] = { 44100, 48000, 96000, 22050, 48000.5 };
    Float64 r = rates[rnd(5)]; int ok = r == 44100 || r == 48000 || r == 96000;
    OSStatus st;
    if (rnd(3)) st = setRate(e->id, r, 8);
    else {
        AudioStreamBasicDescription f = { r, kAudioFormatLinearPCM, kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian,
                                          4 * e->ch, 1, 4 * e->ch, e->ch, 32, 0 };
        st = setFmt(stream(e->id, rnd(2) ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput),
                    rnd(2) ? kAudioStreamPropertyVirtualFormat : kAudioStreamPropertyPhysicalFormat, f);
    }
    LCHECK(e, st == (ok ? 0 : (OSStatus)kAudioDeviceUnsupportedFormatError));
    if (st || !ok) return;
    HDev* p = partner(e); // V and C follow together: the last rate set wins on both
    e->rate = r; if (p) p->rate = r;
}
static void stepLevel(void) {
    HDev* e = randomLive(1); if (!e) return;
    AudioObjectID c[8]; int k = (int)rnd(4); OSStatus st;
    if (ids(e->id, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, c) != 4) { LCHECK(e, 0); return; }
    if (k % 2 == 0) {
        static const Float32 v[] = { 0, 0.25f, 0.5f, 0.75f, 1 }; Float32 s = v[rnd(5)], back = -1;
        st = setBytes(c[k], kAudioLevelControlPropertyScalarValue, 4, &s); LCHECK(e, st == 0);
        if (!st) e->vol[k / 2] = s;
        getBytes(c[k], kAudioLevelControlPropertyScalarValue, 0, &back, 4); LCHECK(e, back == s);
    } else {
        UInt32 m = rnd(2);
        st = setBytes(c[k], kAudioBooleanControlPropertyValue, 4, &m); LCHECK(e, st == 0);
        if (!st) e->mute[k / 2] = m;
        LCHECK(e, u32(c[k], kAudioBooleanControlPropertyValue, 0) == m);
    }
}
// Run (or now and then abort) one queued configuration change in random order; `all`: run every one.
static void stepPerform(int all) {
    for (;;) {
        pthread_mutex_lock(&gQm);
        if (!gQn) { pthread_mutex_unlock(&gQm); return; }
        int i = (int)rnd(gQn); AudioObjectID d = gQ[i].d; UInt64 a = gQ[i].a;
        memmove(&gQ[i], &gQ[i + 1], (size_t)(gQn - i - 1) * sizeof gQ[0]); gQn--;
        pthread_mutex_unlock(&gQm);
        if (!all && rnd(8) == 0) FCHECK(D->AbortDeviceConfigurationChange(gDrv, d, a, NULL) == 0);
        else {
            OSStatus st = D->PerformDeviceConfigurationChange(gDrv, d, a, NULL);
            for (int j = 0; j < gHn; j++) if (gH[j].live && gH[j].id == d) { // the host re-reads the format it changed
                HDev* e = &gH[j]; UInt32 o = chansOf(d, kAudioObjectPropertyScopeOutput);
                LCHECK(e, st == 0 && o && o == chansOf(d, kAudioObjectPropertyScopeInput));
                if (o) e->ch = o;
            }
        }
        if (!all) return;
    }
}
static void stepSleep(void) {
    if (rnd(25)) { usleep(rnd(20000)); return; }
    for (int i = 0; i < 5; i++) { usleep(230000); hpoll(); } // past the driver's 1 s teardown, still watching the list
}

// Random property reads on random objects while everything else runs: sizes agree, nothing past the buffer, CF results +1.
static volatile int gReading;
static int isCF(UInt32 s) {
    return s == kAudioObjectPropertyName || s == kAudioObjectPropertyManufacturer || s == kAudioDevicePropertyDeviceUID || s == kAudioDevicePropertyModelUID
        || s == kAudioPlugInPropertyResourceBundle || s == 'srcf' || s == 'srvr' || s == 'srov';
}
static void* reader(void* arg) {
    uint64_t r = (uint64_t)(uintptr_t)arg * 2654435761u + 1;
    static const UInt32 sels[] = {
        kAudioObjectPropertyBaseClass, kAudioObjectPropertyClass, kAudioObjectPropertyOwner, kAudioObjectPropertyName, kAudioObjectPropertyManufacturer,
        kAudioObjectPropertyOwnedObjects, kAudioPlugInPropertyDeviceList, kAudioPlugInPropertyBoxList, kAudioPlugInPropertyResourceBundle,
        kAudioObjectPropertyCustomPropertyInfoList, 'srcf', 'srvr', 'srov', kAudioDevicePropertyDeviceUID, kAudioDevicePropertyModelUID,
        kAudioDevicePropertyTransportType, kAudioDevicePropertyRelatedDevices, kAudioDevicePropertyClockDomain, kAudioDevicePropertyDeviceIsAlive,
        kAudioDevicePropertyDeviceIsRunning, kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyAvailableNominalSampleRates,
        kAudioDevicePropertyIsHidden, kAudioDevicePropertyZeroTimeStampPeriod, kAudioDevicePropertyPreferredChannelsForStereo,
        kAudioDevicePropertyStreams, kAudioObjectPropertyControlList, kAudioStreamPropertyVirtualFormat, kAudioStreamPropertyPhysicalFormat,
        kAudioStreamPropertyAvailableVirtualFormats, kAudioStreamPropertyAvailablePhysicalFormats, kAudioStreamPropertyDirection,
        kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue, kAudioLevelControlPropertyDecibelRange,
        kAudioLevelControlPropertyConvertScalarToDecibels, kAudioLevelControlPropertyConvertDecibelsToScalar, kAudioBooleanControlPropertyValue,
        kAudioControlPropertyScope, kAudioDevicePropertyLatency, 'xxxx' };
    static const UInt32 scopes[] = { kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput };
    static unsigned char buf[8192 + 512];
    while (gReading) {
        r ^= r << 13; r ^= r >> 7; r ^= r << 17;
        AudioObjectPropertyAddress a = { sels[(r >> 12) % (sizeof sels / sizeof *sels)], scopes[(r >> 20) % 3], 0 };
        AudioObjectID o = (AudioObjectID)(r % 216); UInt32 sz = 0;
        if (D->GetPropertyDataSize(gDrv, o, 0, &a, 0, NULL, &sz)) continue;
        FCHECK(sz <= 8192);
        UInt32 ask = (r >> 28) % 4 ? sz : (UInt32)((r >> 32) % (sz + 1)), got = 0xFFFFFFFF; // sometimes too small
        memset(buf, 0x5A, sizeof buf);
        if (ask >= 4) { Float32 in = 0.5f; memcpy(buf, &in, 4); } // the input of the dB/scalar conversions
        OSStatus e = D->GetPropertyData(gDrv, o, 0, &a, 0, NULL, ask, &got, buf);
        for (size_t k = ask; k < sizeof buf; k++) FCHECK(buf[k] == 0x5A);
        if (e) continue;
        FCHECK(got <= ask);
        if (isCF(a.mSelector)) { CFTypeRef p; memcpy(&p, buf, sizeof p); FCHECK(p); CFRelease(p); }
    }
    return NULL;
}

// Every expected device exists on both sides with the listed channel count, V and C at one rate, nothing queued.
static int settled(void) {
    if (gQn || nDevices() != 2 * (UInt32)gEn) return 0;
    for (int i = 0; i < gEn; i++) {
        char ce[200]; snprintf(ce, sizeof ce, "%s.engine", gE[i].uid);
        AudioObjectID v = byUID(gE[i].uid), c = byUID(ce); UInt32 ch = (UInt32)gE[i].ch;
        if (!v || !c || chansOf(v, kAudioObjectPropertyScopeOutput) != ch || chansOf(v, kAudioObjectPropertyScopeInput) != ch
            || chansOf(c, kAudioObjectPropertyScopeOutput) != ch || chansOf(c, kAudioObjectPropertyScopeInput) != ch || rateOf(v) != rateOf(c)) return 0;
    }
    return 1;
}
static int fuzz(unsigned long long seed, int steps) {
    gSeed = seed; gRng = seed * 0x9E3779B97F4A7C15ull + 0x632BE59BD9B4E019ull; if (!gRng) gRng = 1;
    mach_timebase_info_data_t tb; mach_timebase_info(&tb); gTps = 1e9 * tb.denom / tb.numer;
    gFuzz = 1; gDefer = 1; gQn = 0;
    gEn = 1; strcpy(gE[0].uid, "soundrouter:v1"); strcpy(gE[0].name, "Loopback Audio"); gE[0].ch = 2; // empty storage: the default
    gReading = 1; pthread_t th; FCHECK(pthread_create(&th, NULL, reader, (void*)(uintptr_t)seed) == 0);
    int counts[8] = {0};
    for (gStep = 0; gStep < steps; gStep++) {
        hpoll();
        uint32_t k = rnd(100);
        int s = k < 35 ? 0 : k < 43 ? 1 : k < 53 ? 2 : k < 58 ? 3 : k < 65 ? 4 : k < 72 ? 5 : k < 88 ? 6 : 7;
        counts[s]++;
        switch (s) {
        case 0: stepIO(); break;
        case 1: stepStartStop(); break;
        case 2: stepList(); break;
        case 3: stepBadList(); break;
        case 4: stepRate(); break;
        case 5: stepLevel(); break;
        case 6: stepPerform(0); break;
        default: stepSleep(); break;
        }
    }
    // The app sets its list again on launch (the same list). Then the host runs everything still queued.
    CFArrayRef a = mkList(gE, gEn); FCHECK(setList(a, sizeof a) == 0); CFRelease(a);
    uint64_t t = mach_absolute_time(), ok = 0;
    while (since(t) < 30 && !(ok && since(ok) > 1.5)) { // settled, and still settled 1.5 s later (worker passes done)
        stepPerform(1); hpoll();
        if (!settled()) ok = 0; else if (!ok) ok = mach_absolute_time();
        usleep(20000);
    }
    if (!ok) {
        printf("  not settled: want %d devices, have %u\n", gEn, nDevices());
        for (int i = 0; i < gEn; i++) {
            char ce[200]; snprintf(ce, sizeof ce, "%s.engine", gE[i].uid); AudioObjectID v = byUID(gE[i].uid), c = byUID(ce);
            printf("  %s want %d ch: V %u (%u/%u ch, %.0f Hz) C %u (%u/%u ch, %.0f Hz)\n", gE[i].uid, gE[i].ch, v, chansOf(v, kAudioObjectPropertyScopeInput),
                   chansOf(v, kAudioObjectPropertyScopeOutput), rateOf(v), c, chansOf(c, kAudioObjectPropertyScopeInput), chansOf(c, kAudioObjectPropertyScopeOutput), rateOf(c));
        }
    }
    FCHECK(ok);
    for (int i = 0; i < gEn; i++) {
        char ce[200], cn[300]; snprintf(ce, sizeof ce, "%s.engine", gE[i].uid); snprintf(cn, sizeof cn, "%s (engine)", gE[i].name);
        AudioObjectID v = byUID(gE[i].uid), c = byUID(ce);
        FCHECK(nameIs(v, gE[i].name) && nameIs(c, cn) && !hidden(v) && hidden(c));
        for (int j = 0; j < gHn; j++) {
            HDev* e = &gH[j];
            if (!e->live || e->id != v || !e->known) continue;
            FCHECK(rateOf(v) == (e->rate ? e->rate : 48000)); // the last rate set since the device appeared, else the default
            AudioObjectID cl[8]; Float32 v0 = -1, v1 = -1;
            FCHECK(ids(v, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, cl) == 4);
            getBytes(cl[0], kAudioLevelControlPropertyScalarValue, 0, &v0, 4); getBytes(cl[2], kAudioLevelControlPropertyScalarValue, 0, &v1, 4);
            FCHECK(v0 == e->vol[0] && v1 == e->vol[1]);
            FCHECK(u32(cl[1], kAudioBooleanControlPropertyValue, 0) == e->mute[0] && u32(cl[3], kAudioBooleanControlPropertyValue, 0) == e->mute[1]);
            if (e->started >= 0) FCHECK(u32(v, kAudioDevicePropertyDeviceIsRunning, 0) == (e->started > 0));
        }
    }
    checkSrcf();
    gReading = 0; pthread_join(th, NULL);
    for (int j = 0; j < gHn; j++) for (; gH[j].live && gH[j].started > 0; gH[j].started--) FCHECK(D->StopIO(gDrv, gH[j].id, 1) == 0);
    printf("FUZZ OK seed %llu: %d steps (io %d, start/stop %d, lists %d, bad lists %d, rates %d, levels %d, performs %d, sleeps %d), %d devices\n",
           seed, steps, counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7], gEn);
    return 0;
}

int main(int argc, char** argv) {
    CHECK(argc > 1);
    char bin[1024]; snprintf(bin, sizeof bin, "%s/Contents/MacOS/SoundRouter", argv[1]);
    // 1. factory returns the interface only for the AudioServerPlugIn type UUID.
    void* h = dlopen(bin, RTLD_NOW); CHECK(h);
    void* (*factory)(CFAllocatorRef, CFUUIDRef) = dlsym(h, "SoundRouter_Create"); CHECK(factory);
    CHECK(factory(0, CFUUIDCreateFromString(0, CFSTR("00000000-0000-0000-0000-000000000001"))) == NULL);
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(0, (const UInt8*)argv[1], strlen(argv[1]), true);
    CFPlugInRef p = CFPlugInCreate(0, url); CHECK(p);
    CFArrayRef f = CFPlugInFindFactoriesForPlugInTypeInPlugIn(kAudioServerPlugInTypeUUID, p); CHECK(f && CFArrayGetCount(f) == 1);
    IUnknownVTbl** unk = CFPlugInInstanceCreate(0, CFArrayGetValueAtIndex(f, 0), kAudioServerPlugInTypeUUID); CHECK(unk);
    CHECK((*unk)->QueryInterface(unk, CFUUIDGetUUIDBytes(kAudioServerPlugInDriverInterfaceUUID), (LPVOID*)&gDrv) == 0 && gDrv);
    printf("ok factory type check + QueryInterface\n");
    CHECK(D->Initialize(gDrv, &gHost) == 0);
    CHECK(u32(kAudioObjectPlugInObject, kAudioObjectPropertyClass, 0) == kAudioPlugInClassID);
    if (argc > 3 && !strcmp(argv[2], "fuzz")) return fuzz(strtoull(argv[3], NULL, 10), argc > 4 ? atoi(argv[4]) : 300);

    // 2. default (empty storage): one visible device soundrouter:v1 (2ch) + hidden companion.
    AudioObjectID V = byUID("soundrouter:v1"), C = byUID("soundrouter:v1.engine");
    CHECK(V && C && V != C && nDevices() == 2);
    CHECK(!hidden(V) && hidden(C));
    CHECK(chans(stream(V, kAudioObjectPropertyScopeInput)) == 2 && chans(stream(V, kAudioObjectPropertyScopeOutput)) == 2);
    printf("ok default device V=%u C=%u (spec scheme: V=16 C=20: %s)\n", V, C, V == 16 && C == 20 ? "match" : "DIFFERS");

    // 'srvr' protocol version
    AudioObjectPropertyAddress va = addr('srvr', kAudioObjectPropertyScopeGlobal); CFStringRef ver = NULL; UInt32 sz = sizeof ver;
    CHECK(D->GetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &va, 0, NULL, sz, &sz, &ver) == 0 && ver && CFEqual(ver, CFSTR("1")));
    printf("ok srvr = 1\n");

    // 2b. Initialize restores exactly the stored list, whatever the default would be.
    {
        const char* su[] = { "soundrouter:alpha", "soundrouter:beta" }; const char* sn[] = { "Alpha", "Beta Mic" }; int sc[] = { 3, 5 };
        int w = gStoreWrites;
        reinit(cfg(2, su, sn, sc));
        CHECK(nDevices() == 4 && !byUID("soundrouter:v1") && !byUID("soundrouter:v1.engine") && gStoreWrites == w);
        for (int i = 0; i < 2; i++) {
            char ce[64]; snprintf(ce, sizeof ce, "%s.engine", su[i]);
            AudioObjectID v = byUID(su[i]), c = byUID(ce);
            CHECK(v && c && v != c && !hidden(v) && hidden(c) && nameIs(v, sn[i]));
            CHECK(chans(stream(v, kAudioObjectPropertyScopeInput)) == (UInt32)sc[i] && chans(stream(v, kAudioObjectPropertyScopeOutput)) == (UInt32)sc[i]);
            CHECK(chans(stream(c, kAudioObjectPropertyScopeInput)) == (UInt32)sc[i] && chans(stream(c, kAudioObjectPropertyScopeOutput)) == (UInt32)sc[i]);
        }
        CFArrayRef l = srcf();
        CHECK(CFArrayGetCount(l) == 2 && entryIs(l, 0, su[0], sn[0], 3) && entryIs(l, 1, su[1], sn[1], 5));
        CFRelease(l);
        // A stored empty list means the user removed everything: no default comes back.
        reinit(CFArrayCreate(0, NULL, 0, &kCFTypeArrayCallBacks));
        CHECK(nDevices() == 0);
        printf("ok initialize restores stored list (and an empty one)\n");

        // 2c. Unreadable or invalid stored data gives the default device, never a partial list.
        const char* gu[] = { "soundrouter:good", "soundrouter:bad" }; const char* gn[] = { "Good", "Bad" }; const char* gb[] = { "Good", "   " };
        int g2[] = { 2, 2 }, g0[] = { 2, 0 }, g65[] = { 2, 65 };
        const char* dupu[] = { "soundrouter:good", "soundrouter:good" }; const char* compu[] = { "soundrouter:good", "soundrouter:x.engine" };
        char ub[MAXDEV + 1][32]; const char* u17[MAXDEV + 1]; const char* n17[MAXDEV + 1]; int c17[MAXDEV + 1];
        for (int i = 0; i <= MAXDEV; i++) { snprintf(ub[i], sizeof ub[i], "soundrouter:g%d", i); u17[i] = ub[i]; n17[i] = "G"; c17[i] = 2; }
        int five = 5;
        CFStringRef nk[2] = { CFSTR("uid"), CFSTR("name") }; CFTypeRef nv[2] = { CFSTR("soundrouter:good"), CFSTR("Good") };
        CFDictionaryRef nochan = CFDictionaryCreate(0, (const void**)nk, (const void**)nv, 2, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFTypeRef ncv[1] = { nochan };
        CFStringRef ck[3] = { CFSTR("uid"), CFSTR("name"), CFSTR("channels") }; CFTypeRef strch[3] = { CFSTR("soundrouter:good"), CFSTR("Good"), CFSTR("2") };
        CFDictionaryRef strchan = CFDictionaryCreate(0, (const void**)ck, (const void**)strch, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFTypeRef scv[1] = { strchan };
        CFTypeRef strs[2] = { CFSTR("a"), CFSTR("b") };
        CFTypeRef garbage[] = {
            CFSTR("not a list"), CFNumberCreate(0, kCFNumberIntType, &five), dev("soundrouter:good", "Good", 2), // wrong top-level types
            CFArrayCreate(0, (const void**)strs, 2, &kCFTypeArrayCallBacks),                                       // items are not dictionaries
            CFArrayCreate(0, (const void**)ncv, 1, &kCFTypeArrayCallBacks), CFArrayCreate(0, (const void**)scv, 1, &kCFTypeArrayCallBacks), // missing / non-numeric channels
            cfg(2, gu, gn, g0), cfg(2, gu, gn, g65), cfg(2, gu, gb, g2),                             // 2nd device invalid: nothing applies
            cfg(2, dupu, gn, g2), cfg(2, compu, gn, g2), cfg(MAXDEV + 1, u17, n17, c17),
        };
        for (unsigned i = 0; i < sizeof garbage / sizeof *garbage; i++) {
            printf("  garbage store #%u\n", i);
            reinit(garbage[i]);
            AudioObjectID v = byUID("soundrouter:v1");
            CHECK(nDevices() == 2 && v && byUID("soundrouter:v1.engine") && nameIs(v, "Loopback Audio"));
            CHECK(chans(stream(v, kAudioObjectPropertyScopeOutput)) == 2 && !byUID("soundrouter:good") && !byUID("soundrouter:g0"));
        }
        reinit(NULL); // nothing stored
        CHECK(nDevices() == 2 && byUID("soundrouter:v1") && nameIs(byUID("soundrouter:v1"), "Loopback Audio"));
        printf("ok corrupt / invalid storage falls back to the default device\n");
    }

    // 3. set 2 devices -> both + 2 companions, storage written, notified.
    const char* uid2[] = { "soundrouter:v1", "soundrouter:v2" }; const char* nm2[] = { "Loopback 1", "Loopback 2" }; int ch2[] = { 2, 8 };
    int w0 = gStoreWrites;
    CHECK(setConfig(cfg(2, uid2, nm2, ch2)) == 0);
    CHECK(nDevices() == 4 && gStoreWrites > w0 && gStore && CFArrayGetCount(gStore) == 2 && gNotifies > 0);
    AudioObjectID V2 = byUID("soundrouter:v2"), C2 = byUID("soundrouter:v2.engine");
    CHECK(byUID("soundrouter:v1") && byUID("soundrouter:v1.engine") && V2 && C2 && hidden(C2) && !hidden(V2));
    CHECK(chans(stream(V2, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(C2, kAudioObjectPropertyScopeInput)) == 8);
    AudioObjectPropertyAddress na = addr(kAudioObjectPropertyName, 0); CFStringRef nm = NULL; sz = sizeof nm;
    D->GetPropertyData(gDrv, V2, 0, &na, 0, NULL, sz, &sz, &nm); CHECK(nm && CFEqual(nm, CFSTR("Loopback 2")));
    printf("ok runtime create (4 objects, storage written, notified)\n");

    // 4. rejections leave state untouched.
    const char* dup[] = { "soundrouter:a", "soundrouter:a" }; const char* dn[] = { "A", "B" }; int dc[] = { 2, 2 };
    CHECK(setConfig(cfg(2, dup, dn, dc)) != 0);
    char ub[MAXDEV + 1][32]; const char* uids[MAXDEV + 1]; const char* nms[MAXDEV + 1]; int chs[MAXDEV + 1];
    for (int i = 0; i <= MAXDEV; i++) { snprintf(ub[i], sizeof ub[i], "soundrouter:m%d", i); uids[i] = ub[i]; nms[i] = "M"; chs[i] = 2; }
    CHECK(setConfig(cfg(MAXDEV + 1, uids, nms, chs)) != 0);
    CHECK(nDevices() == 4 && byUID("soundrouter:v2"));
    CHECK(setConfig(cfg(MAXDEV, uids, nms, chs)) == 0 && nDevices() == 2 * MAXDEV); // exactly 16 is fine
    CHECK(setConfig(cfg(2, uid2, nm2, ch2)) == 0 && nDevices() == 4);
    printf("ok duplicate uid / >%d devices rejected\n", MAXDEV);

    // 4b. rename: Name notification for V and C of that slot only, ids unchanged, no device-list churn.
    {
        AudioObjectID V1 = byUID("soundrouter:v1");
        V2 = byUID("soundrouter:v2"); C2 = byUID("soundrouter:v2.engine");
        const char* rn[] = { "Loopback 1", "Renamed 2" };
        gNLn = 0;
        CHECK(setConfig(cfg(2, uid2, rn, ch2)) == 0);
        CHECK(notified(V2, kAudioObjectPropertyName) && notified(C2, kAudioObjectPropertyName));
        CHECK(!notified(V1, kAudioObjectPropertyName) && !notified(kAudioObjectPlugInObject, kAudioPlugInPropertyDeviceList));
        CHECK(byUID("soundrouter:v2") == V2 && byUID("soundrouter:v2.engine") == C2 && nameIs(V2, "Renamed 2") && nameIs(C2, "Renamed 2 (engine)") && nameIs(V1, "Loopback 1"));
        // Setting the same list again does nothing: no notification, no storage write.
        int n0 = gNotifies, w0b = gStoreWrites; CFArrayRef same = cfg(2, uid2, rn, ch2); AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof same, &same) == 0);
        usleep(100000);
        CHECK(gNotifies == n0 && gStoreWrites == w0b);
        // A blank name is invalid and leaves everything as it was.
        const char* bn[] = { "Loopback 1", "   " };
        CHECK(setConfig(cfg(2, uid2, bn, ch2)) != 0 && nameIs(V2, "Renamed 2"));
        printf("ok rename notifies V and C Name only\n");
    }

    // 4c. removing v1 leaves v2's object ids alone; v1's ids are not handed straight back.
    {
        AudioObjectID V1 = byUID("soundrouter:v1"), C1 = byUID("soundrouter:v1.engine");
        V2 = byUID("soundrouter:v2"); C2 = byUID("soundrouter:v2.engine");
        AudioObjectID vi2 = stream(V2, kAudioObjectPropertyScopeInput), vo2 = stream(V2, kAudioObjectPropertyScopeOutput);
        AudioObjectID ci2 = stream(C2, kAudioObjectPropertyScopeInput), co2 = stream(C2, kAudioObjectPropertyScopeOutput);
        AudioObjectID vo1 = stream(V1, kAudioObjectPropertyScopeOutput);
        CHECK(V1 && C1 && vi2 && vo2 && ci2 && co2 && vo1);
        const char* only[] = { "soundrouter:v2" }; const char* on[] = { "Loopback 2" }; int oc[] = { 8 };
        CHECK(setConfig(cfg(1, only, on, oc)) == 0);
        CHECK(nDevices() == 2 && !byUID("soundrouter:v1") && !byUID("soundrouter:v1.engine"));
        CHECK(byUID("soundrouter:v2") == V2 && byUID("soundrouter:v2.engine") == C2);
        CHECK(stream(V2, kAudioObjectPropertyScopeInput) == vi2 && stream(V2, kAudioObjectPropertyScopeOutput) == vo2);
        CHECK(stream(C2, kAudioObjectPropertyScopeInput) == ci2 && stream(C2, kAudioObjectPropertyScopeOutput) == co2);
        CHECK(chans(vo2) == 8 && nameIs(V2, "Loopback 2") && !hidden(V2) && hidden(C2));
        float junk[FR * 2] = {0};
        CHECK(io(V1, vo1, 1, kAudioServerPlugInIOOperationWriteMix, 0, junk) != 0); // I/O on the removed id is refused
        // v2 still carries audio, bit-exact.
        float o8[FR * 8], i8[FR * 8]; fill(o8, 8, 1.0f);
        CHECK(D->StartIO(gDrv, V2, 1) == 0 && D->StartIO(gDrv, C2, 7) == 0);
        CHECK(io(V2, vo2, 1, kAudioServerPlugInIOOperationWriteMix, 500, o8) == 0);
        CHECK(io(C2, ci2, 7, kAudioServerPlugInIOOperationReadInput, 500, memset(i8, 0xAA, sizeof i8)) == 0 && !memcmp(i8, o8, sizeof i8));
        CHECK(D->StopIO(gDrv, V2, 1) == 0 && D->StopIO(gDrv, C2, 7) == 0);
        // Bring v1 back: it gets a different slot than the one it just left, and v2 still keeps its ids.
        CHECK(setConfig(cfg(2, uid2, nm2, ch2)) == 0 && nDevices() == 4);
        CHECK(byUID("soundrouter:v1") && byUID("soundrouter:v1") != V1 && byUID("soundrouter:v1.engine") != C1);
        CHECK(byUID("soundrouter:v2") == V2 && byUID("soundrouter:v2.engine") == C2);
        printf("ok remove v1: v2 ids stable, v1 ids not reused\n");
    }

    // 4d. sample rate and stream format on v2 (8 ch).
    {
        V2 = byUID("soundrouter:v2"); C2 = byUID("soundrouter:v2.engine");
        AudioObjectID vo2 = stream(V2, kAudioObjectPropertyScopeOutput), vi2 = stream(V2, kAudioObjectPropertyScopeInput);
        AudioObjectID ci2 = stream(C2, kAudioObjectPropertyScopeInput), co2 = stream(C2, kAudioObjectPropertyScopeOutput);
        CHECK(rateOf(V2) == 48000 && rateOf(C2) == 48000 && fmtOf(vo2).mSampleRate == 48000);
        // NominalSampleRate 48000 -> 44100 on V asks the host to change V and C; nothing moves before Perform.
        gDefer = 1; gQn = 0;
        CHECK(setRate(V2, 44100, 8) == 0);
        CHECK(gQn == 2 && ((gQ[0].d == V2 && gQ[1].d == C2) || (gQ[0].d == C2 && gQ[1].d == V2)));
        CHECK(rateOf(V2) == 48000 && rateOf(C2) == 48000);
        CHECK(D->PerformDeviceConfigurationChange(gDrv, gQ[0].d, gQ[0].a, NULL) == 0);
        CHECK(rateOf(gQ[0].d) == 44100 && rateOf(gQ[1].d) == 48000); // the HAL changes one device at a time
        CHECK(D->PerformDeviceConfigurationChange(gDrv, gQ[1].d, gQ[1].a, NULL) == 0);
        gDefer = 0;
        CHECK(rateOf(V2) == 44100 && rateOf(C2) == 44100);
        CHECK(fmtOf(vo2).mSampleRate == 44100 && fmtOf(vi2).mSampleRate == 44100 && fmtOf(ci2).mSampleRate == 44100 && fmtOf(co2).mSampleRate == 44100);
        // Setting the rate it already has asks for nothing; bad values are refused and change nothing.
        int c1 = gChanges;
        CHECK(setRate(V2, 44100, 8) == 0 && gChanges == c1);
        CHECK(setRate(V2, 22050, 8) == kAudioDeviceUnsupportedFormatError && setRate(V2, 44100, 4) != 0);
        CHECK(gChanges == c1 && rateOf(V2) == 44100 && rateOf(C2) == 44100);
        // Unsupported stream formats.
        AudioStreamBasicDescription f = fmtOf(vo2), g;
        CHECK(f.mFormatID == kAudioFormatLinearPCM && (f.mFormatFlags & kAudioFormatFlagIsFloat) && f.mBitsPerChannel == 32 && f.mChannelsPerFrame == 8);
        g = f; g.mSampleRate = 22050;
        CHECK(setFmt(vo2, kAudioStreamPropertyVirtualFormat, g) == kAudioDeviceUnsupportedFormatError);
        g = f; g.mFormatFlags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked; g.mBitsPerChannel = 16; g.mBytesPerFrame = g.mBytesPerPacket = 16;
        CHECK(setFmt(vo2, kAudioStreamPropertyVirtualFormat, g) == kAudioDeviceUnsupportedFormatError);
        g = f; g.mBitsPerChannel = 64; g.mBytesPerFrame = g.mBytesPerPacket = 64;
        CHECK(setFmt(vo2, kAudioStreamPropertyPhysicalFormat, g) == kAudioDeviceUnsupportedFormatError);
        g = f; g.mChannelsPerFrame = 2; g.mBytesPerFrame = g.mBytesPerPacket = 8;
        CHECK(setFmt(vo2, kAudioStreamPropertyVirtualFormat, g) == kAudioDeviceUnsupportedFormatError);
        g = f; g.mFormatID = kAudioFormatAppleLossless;
        CHECK(setFmt(ci2, kAudioStreamPropertyVirtualFormat, g) == kAudioDeviceUnsupportedFormatError);
        g = f; g.mFormatFlags &= ~kAudioFormatFlagIsFloat;
        CHECK(setFmt(ci2, kAudioStreamPropertyPhysicalFormat, g) == kAudioDeviceUnsupportedFormatError);
        AudioStreamBasicDescription after = fmtOf(vo2);
        CHECK(gChanges == c1 && !memcmp(&f, &after, sizeof f) && rateOf(V2) == 44100 && rateOf(C2) == 44100);
        // A valid format with another supported rate goes through the same two-device change.
        g = f; g.mSampleRate = 96000;
        CHECK(setFmt(vo2, kAudioStreamPropertyVirtualFormat, g) == 0 && gChanges == c1 + 2);
        CHECK(rateOf(V2) == 96000 && rateOf(C2) == 96000);
        g.mSampleRate = 48000;
        CHECK(setFmt(ci2, kAudioStreamPropertyPhysicalFormat, g) == 0);
        CHECK(rateOf(V2) == 48000 && rateOf(C2) == 48000 && fmtOf(vi2).mSampleRate == 48000);
        // A reconcile pass that runs while a rate change landed on C only must complete it (ask V again), not send C
        // back to V's old rate: that left the pair mismatched, so silent, until the next srcf set.
        gDefer = 1; gQn = 0;
        CHECK(setRate(V2, 44100, 8) == 0 && gQn == 2);
        int qc = gQ[0].d == C2 ? 0 : 1;
        CHECK(gQ[qc].d == C2 && D->PerformDeviceConfigurationChange(gDrv, C2, gQ[qc].a, NULL) == 0);
        CHECK(rateOf(C2) == 44100 && rateOf(V2) == 48000);
        const char* rn2[] = { "Loopback 1", "Rate Two" };
        CHECK(setConfig(cfg(2, uid2, rn2, ch2)) == 0); // a rename: one reconcile pass, which re-asks the laggard
        for (int i = 0; i < 400 && gQn < 3; i++) usleep(5000);
        CHECK(gQn == 3);
        for (int i = 0; i < gQn; i++) if (i != qc) CHECK(D->PerformDeviceConfigurationChange(gDrv, gQ[i].d, gQ[i].a, NULL) == 0);
        gDefer = 0;
        CHECK(rateOf(V2) == 44100 && rateOf(C2) == 44100);
        CHECK(setRate(V2, 48000, 8) == 0 && rateOf(V2) == 48000 && rateOf(C2) == 48000);
        CHECK(setConfig(cfg(2, uid2, nm2, ch2)) == 0 && nameIs(V2, "Loopback 2"));
        // Rapid changes 48k->96k->44.1k->96k->48k while the host holds the Performs back, then runs them FIFO, LIFO, or
        // FIFO after V's first one landed between sets (a stale request after newer sets): the last rate set wins on V and C.
        const Float64 seq[] = { 96000, 44100, 96000, 48000 };
        for (int order = 0; order < 3; order++) {
            gDefer = 1; gQn = 0;
            for (int k = 0; k < 4; k++) {
                CHECK(setRate(V2, seq[k], 8) == 0);
                if (order == 2 && k == 0) performQ(gQ[0].d == V2 ? 0 : 1);
            }
            while (gQn) performQ(order == 1 ? gQn - 1 : 0);
            gDefer = 0;
            printf("  rapid rates, order %d: V2 %.0f C2 %.0f\n", order, rateOf(V2), rateOf(C2));
            CHECK(rateOf(V2) == 48000 && rateOf(C2) == 48000);
        }
        printf("ok sample rate 48k->44.1k->96k->48k on V and C, unsupported formats refused, a pass mid-change completes it,"
               " rapid changes end at the last rate\n");
    }

    // 4e. two srcf sets back to back (no wait): the last list wins, nothing is left over from the others.
    {
        AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        const char* ua[] = { "soundrouter:v1", "soundrouter:v2", "soundrouter:v3" }; const char* na[] = { "One", "Two", "Three" }; int ca[] = { 1, 4, 2 };
        const char* ub2[] = { "soundrouter:v4", "soundrouter:v2", "soundrouter:v5" }; const char* nb[] = { "Four", "Mid Two", "Five" }; int cb[] = { 6, 2, 1 };
        const char* uc[] = { "soundrouter:v2", "soundrouter:v4" }; const char* nc[] = { "Last Two", "Four" }; int cc[] = { 6, 3 };
        for (int i = 0; i < 8; i++) {
            CFArrayRef a = cfg(3, ua, na, ca), b = cfg(3, ub2, nb, cb), c = cfg(2, uc, nc, cc);
            CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof a, &a) == 0);
            CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof b, &b) == 0);
            CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof c, &c) == 0);
        }
        // the final name too: a pass on the middle list can leave 4 devices while v5 waits out a quarantined slot (1 s)
        for (int i = 0; i < 400 && !(nDevices() == 4 && byUID("soundrouter:v4") && !byUID("soundrouter:v1") && nameIs(byUID("soundrouter:v2"), "Last Two")); i++) usleep(10000);
        usleep(300000); // let a straggling reconcile pass finish
        AudioObjectID a2 = byUID("soundrouter:v2"), a4 = byUID("soundrouter:v4");
        CHECK(nDevices() == 4 && a2 && a4 && a2 != a4 && byUID("soundrouter:v2.engine") && byUID("soundrouter:v4.engine"));
        CHECK(!byUID("soundrouter:v1") && !byUID("soundrouter:v3") && !byUID("soundrouter:v5"));
        CHECK(nameIs(a2, "Last Two") && nameIs(a4, "Four"));
        CHECK(chans(stream(a2, kAudioObjectPropertyScopeOutput)) == 6 && chans(stream(a4, kAudioObjectPropertyScopeInput)) == 3);
        CHECK(chans(stream(byUID("soundrouter:v2.engine"), kAudioObjectPropertyScopeInput)) == 6 && chans(stream(byUID("soundrouter:v4.engine"), kAudioObjectPropertyScopeOutput)) == 3);
        CFArrayRef l = srcf();
        CHECK(CFArrayGetCount(l) == 2 && entryIs(l, 0, "soundrouter:v2", "Last Two", 6) && entryIs(l, 1, "soundrouter:v4", "Four", 3));
        CFRelease(l);
        CHECK(gStore && CFArrayGetCount(gStore) == 2 && entryIs(gStore, 0, "soundrouter:v2", "Last Two", 6)); // persisted list is the last one too
        CHECK(setConfig(cfg(2, uid2, nm2, ch2)) == 0 && nDevices() == 4);
        CHECK(chans(stream(byUID("soundrouter:v2"), kAudioObjectPropertyScopeOutput)) == 8);
        printf("ok back-to-back srcf sets end in the last list\n");
    }

    // 5. ring cross-wiring. Output of one side is input of the other, bit-exact, reads never consume.
    V = byUID("soundrouter:v1"); C = byUID("soundrouter:v1.engine");
    AudioObjectID Vi = stream(V, kAudioObjectPropertyScopeInput), Vo = stream(V, kAudioObjectPropertyScopeOutput);
    AudioObjectID Ci = stream(C, kAudioObjectPropertyScopeInput), Co = stream(C, kAudioObjectPropertyScopeOutput);
    CHECK(Vi && Vo && Ci && Co);
    CHECK(D->StartIO(gDrv, V, 1) == 0 && D->StartIO(gDrv, C, 7) == 0);
    Float64 ts; UInt64 ht, seed; CHECK(D->GetZeroTimeStamp(gDrv, V, 1, &ts, &ht, &seed) == 0);
    {   // 7. zero timestamps: period boundaries only, never backwards, same timeline on V and C, host time tracks sample time.
        mach_timebase_info_data_t tb; mach_timebase_info(&tb); double tps = 1e9 * tb.denom / tb.numer;
        Float64 last = ts; UInt64 lasth = ht;
        CHECK(fmod(ts, 16384.0) == 0);
        for (int i = 0; i < 14; i++) {
            usleep(100000);
            Float64 t1; UInt64 h1, s1;
            CHECK(D->GetZeroTimeStamp(gDrv, V, 1, &t1, &h1, &s1) == 0);
            CHECK(t1 >= last && h1 >= lasth && s1 == seed && fmod(t1, 16384.0) == 0);
            last = t1; lasth = h1;
        }
        CHECK(last - ts >= 4 * 16384.0); // >= 1.4 s elapsed = more than 4 periods of 16384/48000 s
        CHECK(fabs((double)(lasth - ht) - (last - ts) / 48000.0 * tps) <= 2.0);
        int same = 0;
        for (int i = 0; i < 20 && !same; i++) {
            Float64 tv, tc, tv2; UInt64 hv, hc, hv2, sv, sc, sv2;
            CHECK(D->GetZeroTimeStamp(gDrv, V, 1, &tv, &hv, &sv) == 0 && D->GetZeroTimeStamp(gDrv, C, 7, &tc, &hc, &sc) == 0 && D->GetZeroTimeStamp(gDrv, V, 1, &tv2, &hv2, &sv2) == 0);
            if (tv == tv2) { CHECK(tc == tv && hc == hv && sc == sv); same = 1; }
        }
        CHECK(same);
        printf("ok zero timestamp monotonic, one period steps, V and C agree\n");
    }
    float out[FR * 2], in[FR * 2], zero[FR * 2] = {0};
    fill(out, 2, 1.0f);
    CHECK(io(V, Vo, 1, kAudioServerPlugInIOOperationWriteMix, 1000, out) == 0);
    CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 1000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, out, sizeof in)); // V.out -> C.in
    CHECK(io(C, Ci, 8, kAudioServerPlugInIOOperationReadInput, 1000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, out, sizeof in)); // 2nd reader, same data
    CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 1000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, out, sizeof in)); // not consumed
    CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 1000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, zero, sizeof in)); // V.in untouched by V.out
    CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 1000 + FR, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, zero, sizeof in)); // past write point = silence
    float out2[FR * 2]; fill(out2, 2, 5000.0f);
    CHECK(io(C, Co, 7, kAudioServerPlugInIOOperationWriteMix, 2000, out2) == 0);
    CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 2000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, out2, sizeof in)); // C.out -> V.in
    CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 2000, memset(in, 0xAA, sizeof in)) == 0 && !memcmp(in, zero, sizeof in));
    CHECK(D->StopIO(gDrv, V, 1) == 0 && D->StopIO(gDrv, C, 7) == 0);
    {   // A restart of I/O re-anchors the timeline: new seed, counting from 0 again.
        Float64 t2; UInt64 h2, s2;
        CHECK(D->StartIO(gDrv, V, 1) == 0 && D->GetZeroTimeStamp(gDrv, V, 1, &t2, &h2, &s2) == 0);
        CHECK(s2 != seed && t2 < 16384.0 * 2);
        CHECK(D->StopIO(gDrv, V, 1) == 0);
    }
    printf("ok ring cross-wiring, shared readers, silence past head\n");
    {   // 5b. one app doing output and input on V, no engine on C: V hears itself. Once C runs, V.in is C.out.
        float o[FR * 2], i2[FR * 2], z[FR * 2] = {0}; fill(o, 2, 3000.0f);
        CHECK(D->StartIO(gDrv, V, 1) == 0);
        CHECK(io(V, Vo, 1, kAudioServerPlugInIOOperationWriteMix, 3000, o) == 0);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 3000, memset(i2, 0xAA, sizeof i2)) == 0 && !memcmp(i2, o, sizeof i2)); // V.out -> V.in
        CHECK(D->StartIO(gDrv, C, 7) == 0);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 3000, memset(i2, 0xAA, sizeof i2)) == 0 && !memcmp(i2, z, sizeof i2)); // engine path: C wrote nothing
        CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 3000, memset(i2, 0xAA, sizeof i2)) == 0 && !memcmp(i2, o, sizeof i2)); // V.out -> C.in
        CHECK(D->StopIO(gDrv, C, 7) == 0);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 3000, memset(i2, 0xAA, sizeof i2)) == 0 && !memcmp(i2, o, sizeof i2)); // direct loop again
        CHECK(D->StopIO(gDrv, V, 1) == 0);
        printf("ok V loops to itself while C is idle, engine path while C runs\n");
    }


    // 5c. V's volume and mute controls (input and output scope), applied on the read side; C has none.
    {
        AudioObjectID ci[4], co[4], cg[8], cc[4], own[8];
        CHECK(ids(V, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeInput, ci) == 2);
        CHECK(ids(V, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeOutput, co) == 2);
        CHECK(ids(V, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, cg) == 4);
        CHECK(ids(V, kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal, own) == 6); // 2 streams + 4 controls
        CHECK(ids(V, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeGlobal, own) == 2);
        CHECK(ids(C, kAudioObjectPropertyControlList, kAudioObjectPropertyScopeGlobal, cc) == 0);
        AudioObjectID vIn = ci[0], mIn = ci[1], vOut = co[0], mOut = co[1];
        CHECK(u32(vOut, kAudioObjectPropertyClass, 0) == kAudioVolumeControlClassID && u32(vOut, kAudioObjectPropertyBaseClass, 0) == kAudioLevelControlClassID);
        CHECK(u32(mOut, kAudioObjectPropertyClass, 0) == kAudioMuteControlClassID && u32(mOut, kAudioObjectPropertyBaseClass, 0) == kAudioBooleanControlClassID);
        CHECK(u32(vIn, kAudioObjectPropertyOwner, 0) == V && u32(vIn, kAudioControlPropertyScope, 0) == kAudioObjectPropertyScopeInput);
        CHECK(u32(vOut, kAudioControlPropertyScope, 0) == kAudioObjectPropertyScopeOutput && u32(vOut, kAudioControlPropertyElement, 0) == kAudioObjectPropertyElementMain);
        // Defaults: full volume, unmuted.
        CHECK(f32(vOut, kAudioLevelControlPropertyScalarValue) == 1.0f && f32(vOut, kAudioLevelControlPropertyDecibelValue) == 0.0f);
        CHECK(f32(vIn, kAudioLevelControlPropertyScalarValue) == 1.0f && u32(mIn, kAudioBooleanControlPropertyValue, 0) == 0 && u32(mOut, kAudioBooleanControlPropertyValue, 0) == 0);
        AudioValueRange rg; CHECK(getBytes(vOut, kAudioLevelControlPropertyDecibelRange, 0, &rg, sizeof rg) == 0 && rg.mMinimum == -64 && rg.mMaximum == 0);
        CHECK(convert(vOut, kAudioLevelControlPropertyConvertScalarToDecibels, 0.5f) == -32.0f && convert(vOut, kAudioLevelControlPropertyConvertDecibelsToScalar, -16.0f) == 0.75f);
        // Settable: values yes, class/range no; wrong sizes refused.
        Boolean st = 0; AudioObjectPropertyAddress sa = addr(kAudioLevelControlPropertyScalarValue, 0);
        CHECK(D->IsPropertySettable(gDrv, vOut, 0, &sa, &st) == 0 && st);
        sa = addr(kAudioLevelControlPropertyDecibelRange, 0); CHECK(D->IsPropertySettable(gDrv, vOut, 0, &sa, &st) == 0 && !st);
        sa = addr(kAudioBooleanControlPropertyValue, 0); CHECK(D->IsPropertySettable(gDrv, mOut, 0, &sa, &st) == 0 && st);
        Float64 bad = 0.5; CHECK(setBytes(vOut, kAudioLevelControlPropertyScalarValue, 8, &bad) != 0 && setBytes(mOut, kAudioBooleanControlPropertyValue, 1, &bad) != 0);
        CHECK(setBytes(vOut, kAudioLevelControlPropertyDecibelRange, 16, &rg) != 0);
        CHECK(f32(vOut, kAudioLevelControlPropertyScalarValue) == 1.0f && u32(mOut, kAudioBooleanControlPropertyValue, 0) == 0);
        // Writes land, convert both ways, clamp, and notify.
        gNLn = 0;
        setF32(vOut, kAudioLevelControlPropertyScalarValue, 0.5f);
        CHECK(f32(vOut, kAudioLevelControlPropertyDecibelValue) == -32.0f && f32(vOut, kAudioLevelControlPropertyScalarValue) == 0.5f);
        CHECK(notified(vOut, kAudioLevelControlPropertyScalarValue) && notified(vOut, kAudioLevelControlPropertyDecibelValue));
        setF32(vOut, kAudioLevelControlPropertyDecibelValue, -200.0f); CHECK(f32(vOut, kAudioLevelControlPropertyScalarValue) == 0.0f && f32(vOut, kAudioLevelControlPropertyDecibelValue) == -64.0f);
        setF32(vOut, kAudioLevelControlPropertyScalarValue, 7.0f); CHECK(f32(vOut, kAudioLevelControlPropertyDecibelValue) == 0.0f);
        gNLn = 0; setMute(mOut, 1); CHECK(u32(mOut, kAudioBooleanControlPropertyValue, 0) == 1 && notified(mOut, kAudioBooleanControlPropertyValue)); setMute(mOut, 0);
        CHECK(u32(mIn, kAudioBooleanControlPropertyValue, 0) == 0 && f32(vIn, kAudioLevelControlPropertyScalarValue) == 1.0f); // scopes independent

        // Gain on the data: -6.0206 dB on the output scope, bit-exact against one float multiply by the same factor.
        const Float32 DB = -6.0206f; setF32(vOut, kAudioLevelControlPropertyDecibelValue, DB);
        CHECK(f32(vOut, kAudioLevelControlPropertyDecibelValue) == DB);
        Float32 g = gainOf(DB), gi, gg; CHECK(fabsf(g - 0.5f) < 1e-4f);
        float raw[FR * 2], want[FR * 2], rd[FR * 2], zr[FR * 2] = {0}; fill(raw, 2, 1.0f);
        #define SCALED(dst, src, k) for (int q = 0; q < FR * 2; q++) (dst)[q] = (src)[q] * (k)
        // Engine path: V.out -> C.in is scaled; the next read after resetting to 0 dB is raw again (stored data untouched).
        CHECK(D->StartIO(gDrv, V, 1) == 0 && D->StartIO(gDrv, C, 7) == 0);
        CHECK(io(V, Vo, 1, kAudioServerPlugInIOOperationWriteMix, 8000, raw) == 0);
        SCALED(want, raw, g);
        CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, want, sizeof rd));
        setMute(mOut, 1);
        CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, zr, sizeof rd));
        setMute(mOut, 0); setF32(vOut, kAudioLevelControlPropertyScalarValue, 1.0f);
        CHECK(io(C, Ci, 7, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, raw, sizeof rd));
        // V.in via the engine (ring B) carries the input scope only; the output level does not leak into it, and C.out is stored raw.
        setF32(vOut, kAudioLevelControlPropertyDecibelValue, DB);
        fill(want, 2, 9000.0f);
        CHECK(io(C, Co, 7, kAudioServerPlugInIOOperationWriteMix, 9000, want) == 0);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 9000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, want, sizeof rd));
        setF32(vIn, kAudioLevelControlPropertyDecibelValue, -12.0f); gi = gainOf(-12.0f);
        float w2[FR * 2]; SCALED(w2, want, gi);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 9000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, w2, sizeof rd));
        setMute(mIn, 1);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 9000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, zr, sizeof rd));
        setMute(mIn, 0);
        // V -> V loop (C idle): output level times input level, one multiply by their product.
        CHECK(D->StopIO(gDrv, C, 7) == 0);
        gg = gi * g; SCALED(w2, raw, gg);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, w2, sizeof rd));
        setMute(mOut, 1);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, zr, sizeof rd));
        setMute(mOut, 0); setF32(vIn, kAudioLevelControlPropertyScalarValue, 1.0f); setF32(vOut, kAudioLevelControlPropertyScalarValue, 1.0f);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 8000, memset(rd, 0xAA, sizeof rd)) == 0 && !memcmp(rd, raw, sizeof rd)); // back to bit-exact unity

        // Overload counter: a WriteMix scheduled before the device's current time is counted per device; reads and on-time writes are not.
        CHECK(overloads(V) == 0 && overloads(C) == 0);
        CHECK(io(V, Vo, 1, kAudioServerPlugInIOOperationWriteMix, 8000, raw) == 0 && overloads(V) == 0); // on time (current time 0)
        gCi.mCurrentTime.mSampleTime = 1e6;
        CHECK(io(V, Vo, 1, kAudioServerPlugInIOOperationWriteMix, 8000, raw) == 0 && overloads(V) == 1 && overloads(C) == 0);
        CHECK(io(V, Vi, 1, kAudioServerPlugInIOOperationReadInput, 8000, rd) == 0 && overloads(V) == 1);
        gCi.mCurrentTime.mSampleTime = 0;
        {   // the property is advertised as a custom property, read-only
            AudioObjectPropertyAddress la = addr(kAudioObjectPropertyCustomPropertyInfoList, 0); UInt32 lsz = 0; AudioServerPlugInCustomPropertyInfo inf[2];
            CHECK(D->GetPropertyDataSize(gDrv, V, 0, &la, 0, NULL, &lsz) == 0 && lsz == sizeof inf[0]);
            CHECK(D->GetPropertyData(gDrv, V, 0, &la, 0, NULL, lsz, &lsz, inf) == 0 && inf[0].mSelector == 'srov');
            AudioObjectPropertyAddress oa = addr('srov', 0); Boolean os = 1;
            CHECK(D->IsPropertySettable(gDrv, V, 0, &oa, &os) == 0 && !os);
        }
        CHECK(D->StopIO(gDrv, V, 1) == 0);
        // CanBeDefault: V can be the default device but not the system-sound device; C never.
        CHECK(u32(V, kAudioDevicePropertyDeviceCanBeDefaultDevice, 0) == 1 && u32(V, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, 0) == 0);
        CHECK(u32(C, kAudioDevicePropertyDeviceCanBeDefaultDevice, 0) == 0 && u32(C, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, 0) == 0);
        printf("ok level/mute controls: gain bit-exact on engine path and V loop, mute zeros, overload counter, default-device flags\n");
    }

    // 8. WillDoIOOperation: only ReadInput and WriteMix, in place. Other ops are no-ops even if the host calls them.
    {
        UInt32 ops[] = { kAudioServerPlugInIOOperationThread, kAudioServerPlugInIOOperationCycle, kAudioServerPlugInIOOperationReadInput,
                         kAudioServerPlugInIOOperationConvertInput, kAudioServerPlugInIOOperationProcessInput, kAudioServerPlugInIOOperationProcessOutput,
                         kAudioServerPlugInIOOperationMixOutput, kAudioServerPlugInIOOperationProcessMix, kAudioServerPlugInIOOperationConvertMix,
                         kAudioServerPlugInIOOperationWriteMix };
        AudioObjectID devs[] = { V, C }; AudioObjectID sts[] = { Vi, Ci };
        for (int d = 0; d < 2; d++) for (unsigned i = 0; i < sizeof ops / sizeof *ops; i++) {
            Boolean will = 7, inPlace = 7; int want = ops[i] == kAudioServerPlugInIOOperationReadInput || ops[i] == kAudioServerPlugInIOOperationWriteMix;
            CHECK(D->WillDoIOOperation(gDrv, devs[d], 1, ops[i], &will, &inPlace) == 0 && will == want && inPlace == 1);
            if (!want) {
                unsigned char b[FR * 2 * sizeof(float)]; memset(b, 0xAA, sizeof b);
                CHECK(io(devs[d], sts[d], 1, ops[i], 1000, (float*)b) == 0);
                for (unsigned k = 0; k < sizeof b; k++) CHECK(b[k] == 0xAA);
            }
        }
        printf("ok WillDoIOOperation answers ReadInput/WriteMix only\n");
    }

    // 6. channel change 8 -> 16 via RequestDeviceConfigurationChange -> PerformDeviceConfigurationChange.
    int c0 = gChanges; int ch3[] = { 2, 16 };
    CHECK(setConfig(cfg(2, uid2, nm2, ch3)) == 0 && gChanges > c0);
    V2 = byUID("soundrouter:v2"); C2 = byUID("soundrouter:v2.engine");
    CHECK(V2 && C2 && chans(stream(V2, kAudioObjectPropertyScopeOutput)) == 16 && chans(stream(C2, kAudioObjectPropertyScopeInput)) == 16);
    printf("ok channel change 8->16\n");

    // 6a. Rapid channel changes on v1 (2 ch) while the host holds the Performs back, then runs them FIFO or newest
    // first, with everything deferred or with V's first request performed between sets (a later set back to V's count
    // must still ask, and stale requests land after newer sets): V and C end at the last count set.
    {
        const int seqs[2][4] = { { 8, 16, 8 }, { 8, 16, 8, 16 } }, lens[2] = { 3, 4 };
        AudioObjectID V1 = byUID("soundrouter:v1"), C1 = byUID("soundrouter:v1.engine");
        int bad = 0;
        for (int q = 0; q < 2; q++) for (int order = 0; order < 4; order++) {
            gDefer = 1; gQn = 0;
            for (int k = 0; k < lens[q]; k++) {
                char nm[32]; snprintf(nm, sizeof nm, "Chan %d.%d.%d", q, order, k); const char* rn[] = { nm, "Loopback 2" };
                int ch[] = { seqs[q][k], 16 }, n0 = gQn;
                CHECK(setConfig(cfg(2, uid2, rn, ch)) == 0); // renamed too, so the pass notifies and setConfig waits for it
                for (int i = 0; i < 100 && gQn < n0 + 2; i++) usleep(5000);
                if (order >= 2 && k == 0) performQ(gQ[0].d == V1 ? 0 : 1);
            }
            int n = gQn;
            while (gQn) performQ(order & 1 ? gQn - 1 : 0);
            gDefer = 0;
            UInt32 v = chans(stream(V1, kAudioObjectPropertyScopeOutput)), c = chans(stream(C1, kAudioObjectPropertyScopeInput)), want = seqs[q][lens[q] - 1];
            printf("  rapid channels %d, order %d: V1 %u C1 %u, want %u (%d requests)\n", q, order, v, c, want, n);
            bad |= v != want || c != want || chans(stream(V1, kAudioObjectPropertyScopeInput)) != want || chans(stream(C1, kAudioObjectPropertyScopeOutput)) != want;
            CHECK(setConfig(cfg(2, uid2, nm2, ch3)) == 0); // back to 2 ch, performed inline
            CHECK(chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 2 && chans(stream(C1, kAudioObjectPropertyScopeInput)) == 2);
        }
        CHECK(!bad);
        // The count both sides already run asks for nothing.
        int c1 = gChanges; const char* rn[] = { "Same Count", "Loopback 2" };
        CHECK(setConfig(cfg(2, uid2, rn, ch3)) == 0 && setConfig(cfg(2, uid2, nm2, ch3)) == 0 && gChanges == c1);
        printf("ok rapid channel changes 2->8->16->8(->16) end at the last count on V and C in any Perform order\n");
    }

    // 6b. regression (heap overflow in coreaudiod): the HAL applies a channel change per device. After V changed
    // 2 -> 8 but before C did, I/O on C (still 2 ch) must stay inside its buffer and read silence.
    {
        int ch4[] = { 8, 16 }; gDefer = 1; gQn = 0;
        CFArrayRef want = cfg(2, uid2, nm2, ch4); AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof want, &want) == 0);
        for (int i = 0; i < 400 && gQn < 2; i++) usleep(5000); // only v1 changes: its V and C
        usleep(30000); CHECK(gQn == 2);
        AudioObjectID V1 = byUID("soundrouter:v1"), C1 = byUID("soundrouter:v1.engine");
        CHECK(V1 && C1 && chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 2);
        int qv = -1, qc = -1;
        for (int i = 0; i < gQn; i++) { if (gQ[i].d == V1) qv = i; if (gQ[i].d == C1) qc = i; }
        CHECK(qv >= 0 && qc >= 0);
        CHECK(D->PerformDeviceConfigurationChange(gDrv, V1, gQ[qv].a, NULL) == 0); // only V1 is 8 ch now
        CHECK(chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(C1, kAudioObjectPropertyScopeInput)) == 2);
        enum { G = 4096 };
        float v8[FR * 8], cb[FR * 2 + G]; fill(v8, 8, 1.0f);
        CHECK(io(V1, stream(V1, kAudioObjectPropertyScopeOutput), 1, kAudioServerPlugInIOOperationWriteMix, 3000, v8) == 0);
        for (int i = 0; i < FR * 2 + G; i++) cb[i] = -777.0f;
        CHECK(io(C1, stream(C1, kAudioObjectPropertyScopeInput), 7, kAudioServerPlugInIOOperationReadInput, 3000, cb) == 0);
        for (int i = 0; i < FR * 2; i++) CHECK(cb[i] == 0.0f);   // mismatched pair is silent
        for (int i = FR * 2; i < FR * 2 + G; i++) CHECK(cb[i] == -777.0f); // guard words untouched
        CHECK(D->PerformDeviceConfigurationChange(gDrv, C1, gQ[qc].a, NULL) == 0);
        for (int i = 0; i < gQn; i++) if (i != qv && i != qc) CHECK(D->PerformDeviceConfigurationChange(gDrv, gQ[i].d, gQ[i].a, NULL) == 0);
        gDefer = 0;
        float c8[FR * 8];
        CHECK(io(V1, stream(V1, kAudioObjectPropertyScopeOutput), 1, kAudioServerPlugInIOOperationWriteMix, 4000, v8) == 0);
        CHECK(io(C1, stream(C1, kAudioObjectPropertyScopeInput), 7, kAudioServerPlugInIOOperationReadInput, 4000, memset(c8, 0xAA, sizeof c8)) == 0 && !memcmp(c8, v8, sizeof c8));
        printf("ok mismatched V/C channel change stays in bounds, then bit-exact at 8 ch\n");
    }

    // 6e. regression: the host aborted V's channel change but performed C's (V 8 ch, C 2 ch), so the pair is silent.
    // Setting the same list again (as the app does on launch) must re-ask the laggard, not return early and leave the
    // pair mismatched until some other list comes along.
    {
        AudioObjectID V1 = byUID("soundrouter:v1"), C1 = byUID("soundrouter:v1.engine");
        CHECK(chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(C1, kAudioObjectPropertyScopeInput)) == 8);
        AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        CFArrayRef want = cfg(2, uid2, nm2, ch3);
        gDefer = 1; gQn = 0;
        CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof want, &want) == 0);
        for (int i = 0; i < 400 && gQn < 2; i++) usleep(5000);
        usleep(30000); CHECK(gQn == 2);
        int qv = gQ[0].d == V1 ? 0 : 1;
        CHECK(gQ[qv].d == V1 && gQ[1 - qv].d == C1);
        CHECK(D->AbortDeviceConfigurationChange(gDrv, V1, gQ[qv].a, NULL) == 0);
        CHECK(D->PerformDeviceConfigurationChange(gDrv, C1, gQ[1 - qv].a, NULL) == 0);
        gQn = 0;
        CHECK(chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(C1, kAudioObjectPropertyScopeInput)) == 2);
        int w = gStoreWrites; CFArrayRef again = cfg(2, uid2, nm2, ch3);
        CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof again, &again) == 0);
        for (int i = 0; i < 400 && gQn < 1; i++) usleep(5000);
        usleep(30000);
        CHECK(gQn == 1 && gQ[0].d == V1 && gStoreWrites == w); // the laggard only; nothing new stored
        performQ(0); gDefer = 0;
        CHECK(chans(stream(V1, kAudioObjectPropertyScopeOutput)) == 2 && chans(stream(V1, kAudioObjectPropertyScopeInput)) == 2);
        CHECK(chans(stream(C1, kAudioObjectPropertyScopeOutput)) == 2 && chans(stream(C1, kAudioObjectPropertyScopeInput)) == 2);
        printf("ok the same list set again completes a channel change the host aborted on one side\n");
    }

    // 6c. regression (heap overflow in coreaudiod): with all 16 slots used, swapping one device for a wider one must
    // not hand the freed slot to the new device in the same pass. Until the removal had time to settle, the old ids
    // must not exist as the new device, and a 2-channel ReadInput on them must stay inside its buffer.
    {
        char u[MAXDEV][24], n[MAXDEV][16]; const char* up[MAXDEV]; const char* np[MAXDEV]; int cc[MAXDEV];
        for (int i = 0; i < MAXDEV; i++) { snprintf(u[i], 24, "soundrouter:sw%d", i); snprintf(n[i], 16, "Sw %d", i); up[i] = u[i]; np[i] = n[i]; cc[i] = 2; }
        CHECK(setConfig(CFArrayCreate(0, NULL, 0, &kCFTypeArrayCallBacks)) == 0);
        for (int i = 0; i < 400 && nDevices() != 0; i++) usleep(10000);
        usleep(1500000); // removal pass + any straggler
        CHECK(setConfig(cfg(MAXDEV, up, np, cc)) == 0);
        for (int i = 0; i < 400 && !byUID("soundrouter:sw15"); i++) usleep(10000);
        AudioObjectID oldV = byUID("soundrouter:sw15"), oldC = byUID("soundrouter:sw15.engine");
        CHECK(nDevices() == 2 * MAXDEV && oldV && oldC);
        gStaleDev[0] = oldV; gStaleDev[1] = oldC;
        gStaleSt[0] = stream(oldV, kAudioObjectPropertyScopeInput); gStaleSt[1] = stream(oldC, kAudioObjectPropertyScopeInput);
        CHECK(gStaleSt[0] && gStaleSt[1]);
        up[15] = "soundrouter:swide"; np[15] = "Wide"; cc[15] = 8;
        AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        CFArrayRef want = cfg(MAXDEV, up, np, cc);
        gProbe = 1; // gapProbe runs on the worker when it announces the removal: that is where the gap is observed
        CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof want, &want) == 0);
        // Poll the stale ids while the old 2-channel device lives, through the gap, up to the re-creation. Once the freed
        // slot is legitimately re-created as the 8-channel sw:wide, a 2-channel read on its old ids writes 8 channels:
        // that is this test's own misuse, and it can start between the loop condition and the read. sw:wide never goes
        // away again, so "absent after the read" proves it was absent during it; only then must the guard words be intact.
        for (int i = 0; i < 400 && !byUID("soundrouter:swide"); i++) {
            for (int k = 0; k < 2; k++) {
                int clean = staleReadClean(k);
                if (!byUID("soundrouter:swide")) CHECK(clean);
            }
            usleep(5000);
        }
        CHECK(gGap); // the add came in a later pass than the removal
        for (int i = 0; i < 400 && !byUID("soundrouter:swide"); i++) usleep(10000);
        AudioObjectID w = byUID("soundrouter:swide");
        CHECK(w && nDevices() == 2 * MAXDEV && chans(stream(w, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(byUID("soundrouter:swide.engine"), kAudioObjectPropertyScopeInput)) == 8);
        gProbe = 0;
        printf("ok full-pool swap: freed slot not reused in the same pass, no overrun\n");
    }

    // 6d. regression (heap overflow in coreaudiod): a storm of srcf sets on a full pool, each swapping the last device
    // for a wider one (4, 6, 8 ch). A reconcile must not hand out a slot that another one freed milliseconds earlier:
    // within the teardown time the HAL may still run I/O on the old ids with the old (2-channel) buffer.
    {
        char u[MAXDEV][24], n[MAXDEV][16]; const char* up[MAXDEV]; const char* np[MAXDEV]; int cc[MAXDEV];
        for (int i = 0; i < MAXDEV; i++) { snprintf(u[i], 24, "soundrouter:sw%d", i); snprintf(n[i], 16, "Sw %d", i); up[i] = u[i]; np[i] = n[i]; cc[i] = 2; }
        CHECK(setConfig(cfg(MAXDEV, up, np, cc)) == 0); // sw:wide (8 ch) back to a 2-channel sw:15
        for (int i = 0; i < 400 && !byUID("soundrouter:sw15"); i++) usleep(10000);
        AudioObjectID oldV = byUID("soundrouter:sw15"), oldC = byUID("soundrouter:sw15.engine");
        CHECK(nDevices() == 2 * MAXDEV && oldV && oldC && chans(stream(oldV, kAudioObjectPropertyScopeInput)) == 2);
        gStaleDev[0] = oldV; gStaleDev[1] = oldC;
        gStaleSt[0] = stream(oldV, kAudioObjectPropertyScopeInput); gStaleSt[1] = stream(oldC, kAudioObjectPropertyScopeInput);
        CHECK(gStaleSt[0] && gStaleSt[1] && staleReadClean(0) && staleReadClean(1));
        mach_timebase_info_data_t tb; mach_timebase_info(&tb); gTps = 1e9 * tb.denom / tb.numer;
        AudioObjectPropertyAddress sa = addr('srcf', kAudioObjectPropertyScopeGlobal);
        const char* su[] = { "soundrouter:st4", "soundrouter:st6", "soundrouter:st8" }; const char* sn[] = { "St 4", "St 6", "St 8" };
        gStormT0 = mach_absolute_time(); gStorm = 1;
        for (int k = 0; k < 3; k++) {
            up[15] = su[k]; np[15] = sn[k]; cc[15] = 4 + 2 * k;
            CFArrayRef want = cfg(MAXDEV, up, np, cc);
            CHECK(D->SetPropertyData(gDrv, kAudioObjectPlugInObject, 0, &sa, 0, NULL, sizeof want, &want) == 0);
            usleep(2000);
        }
        for (int i = 0; i < 400 && !byUID("soundrouter:st8"); i++) usleep(10000);
        usleep(300000); // a straggling pass would announce here
        gStorm = 0;
        AudioObjectID w = byUID("soundrouter:st8");
        CHECK(w && nDevices() == 2 * MAXDEV && !byUID("soundrouter:st4") && !byUID("soundrouter:st6") && !byUID("soundrouter:sw15"));
        CHECK(chans(stream(w, kAudioObjectPropertyScopeOutput)) == 8 && chans(stream(byUID("soundrouter:st8.engine"), kAudioObjectPropertyScopeInput)) == 8);
        CHECK(gStormNotes > 0 && gStormThreads < 2); // reconciles ran on a single worker thread
        printf("ok srcf storm on a full pool: no slot reused before teardown, one reconcile thread\n");
    }

    // 7. remove all devices.
    CHECK(setConfig(CFArrayCreate(0, NULL, 0, &kCFTypeArrayCallBacks)) == 0);
    CHECK(nDevices() == 0 && !byUID("soundrouter:v1") && gStore && CFArrayGetCount(gStore) == 0);
    printf("ok remove all\n");
    printf("ALL OK (notifications=%d)\n", gNotifies);
    return 0;
}
