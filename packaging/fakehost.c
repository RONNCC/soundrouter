// Fake coreaudiod host: loads SoundRouter.driver via CFPlugIn like the HAL and drives the vtable. No sudo.
// usage: fakehost <path/to/SoundRouter.driver>. Prints "ALL OK" or the first failing assertion (exit 1).
#include <CoreAudio/AudioServerPlugIn.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define CHECK(c) do { if (!(c)) { printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); exit(1); } } while (0)
#define MAXDEV 16 // sr_core::driver_proto::MAX_DEVICES
#define FR 256

static CFPropertyListRef gStore;
static volatile int gNotifies, gStoreWrites;
static AudioServerPlugInDriverRef gDrv;
#define D (*gDrv)

static OSStatus PropsChanged(AudioServerPlugInHostRef h, AudioObjectID o, UInt32 n, const AudioObjectPropertyAddress* a) {
    for (UInt32 i = 0; i < n; i++) {
        UInt32 s = CFSwapInt32HostToBig(a[i].mSelector);
        printf("  host.PropertiesChanged obj=%u sel='%.4s'\n", o, (char*)&s);
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
static struct { AudioObjectID d; UInt64 a; } gQ[8]; // requests held back while gDefer, to run them one device at a time
static OSStatus ReqChange(AudioServerPlugInHostRef h, AudioObjectID d, UInt64 act, void* info) {
    if (gDefer) { if (gQn < 8) { gQ[gQn].d = d; gQ[gQn++].a = act; } return 0; }
    printf("  host.RequestDeviceConfigurationChange dev=%u action=%llu -> Perform\n", d, act);
    gChanges++;
    return D->PerformDeviceConfigurationChange(gDrv, d, act, info); // real host stops IO first
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

static AudioServerPlugInIOCycleInfo gCi;
static OSStatus io(AudioObjectID d, AudioObjectID st, UInt32 client, int op, double t, float* buf) {
    gCi.mInputTime.mSampleTime = gCi.mOutputTime.mSampleTime = t;
    return D->DoIOOperation(gDrv, d, st, client, op, FR, &gCi, buf, NULL);
}
static void fill(float* b, int ch, float base) { for (int i = 0; i < FR * ch; i++) b[i] = base + (float)i; }

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

    // 5. ring cross-wiring. Output of one side is input of the other, bit-exact, reads never consume.
    V = byUID("soundrouter:v1"); C = byUID("soundrouter:v1.engine");
    AudioObjectID Vi = stream(V, kAudioObjectPropertyScopeInput), Vo = stream(V, kAudioObjectPropertyScopeOutput);
    AudioObjectID Ci = stream(C, kAudioObjectPropertyScopeInput), Co = stream(C, kAudioObjectPropertyScopeOutput);
    CHECK(Vi && Vo && Ci && Co);
    CHECK(D->StartIO(gDrv, V, 1) == 0 && D->StartIO(gDrv, C, 7) == 0);
    Float64 ts; UInt64 ht, seed; CHECK(D->GetZeroTimeStamp(gDrv, V, 1, &ts, &ht, &seed) == 0);
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
    printf("ok ring cross-wiring, shared readers, silence past head\n");

    // 6. channel change 8 -> 16 via RequestDeviceConfigurationChange -> PerformDeviceConfigurationChange.
    int c0 = gChanges; int ch3[] = { 2, 16 };
    CHECK(setConfig(cfg(2, uid2, nm2, ch3)) == 0 && gChanges > c0);
    V2 = byUID("soundrouter:v2"); C2 = byUID("soundrouter:v2.engine");
    CHECK(V2 && C2 && chans(stream(V2, kAudioObjectPropertyScopeOutput)) == 16 && chans(stream(C2, kAudioObjectPropertyScopeInput)) == 16);
    printf("ok channel change 8->16\n");

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

    // 7. remove all devices.
    CHECK(setConfig(CFArrayCreate(0, NULL, 0, &kCFTypeArrayCallBacks)) == 0);
    CHECK(nDevices() == 0 && !byUID("soundrouter:v1") && gStore && CFArrayGetCount(gStore) == 0);
    printf("ok remove all\n");
    printf("ALL OK (notifications=%d)\n", gNotifies);
    return 0;
}
