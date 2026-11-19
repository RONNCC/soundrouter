// Fake coreaudiod host: loads SoundRouter.driver via CFPlugIn like the HAL and drives the vtable. No sudo.
// usage: fakehost <path/to/SoundRouter.driver>. Prints "ALL OK" or the first failing assertion (exit 1).
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
static void gapProbe(void);
static void stormProbe(void);

static OSStatus PropsChanged(AudioServerPlugInHostRef h, AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress* a) {
    for (UInt32 i = 0; i < n; i++) {
        UInt32 s = CFSwapInt32HostToBig(a[i].mSelector);
        printf("  host.PropertiesChanged obj=%u sel='%.4s'\n", o, (char*)&s);
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
static struct { AudioObjectID d; UInt64 a; } gQ[32]; // requests held back while gDefer, to run them one device at a time
static OSStatus ReqChange(AudioServerPlugInHostRef h, AudioObjectID d, UInt64 act, void* info) {
    if (gDefer) { if (gQn < 32) { gQ[gQn].d = d; gQ[gQn++].a = act; } return 0; }
    printf("  host.RequestDeviceConfigurationChange dev=%u action=%llu -> Perform\n", d, act);
    gChanges++;
    return D->PerformDeviceConfigurationChange(gDrv, d, act, info); // real host stops IO first
}
// Perform held-back request i and drop it from the queue.
static void performQ(int i) {
    CHECK(i < gQn && D->PerformDeviceConfigurationChange(gDrv, gQ[i].d, gQ[i].a, NULL) == 0);
    memmove(&gQ[i], &gQ[i + 1], (size_t)(gQn - i - 1) * sizeof gQ[0]); gQn--;
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
