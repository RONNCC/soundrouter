#include <CoreAudio/CoreAudio.h>
#include <stdio.h>
#include <stdlib.h>
static UInt32 u32(AudioObjectID d,AudioObjectPropertySelector s,AudioObjectPropertyScope sc){AudioObjectPropertyAddress a={s,sc,0};UInt32 v=0,n=4;AudioObjectGetPropertyData(d,&a,0,0,&n,&v);return v;}
int main(){AudioObjectPropertyAddress a={kAudioHardwarePropertyDevices,kAudioObjectPropertyScopeGlobal,0};UInt32 n;AudioObjectGetPropertyDataSize(kAudioObjectSystemObject,&a,0,0,&n);
 AudioObjectID*ids=malloc(n);AudioObjectGetPropertyData(kAudioObjectSystemObject,&a,0,0,&n,ids);
 for(UInt32 i=0;i<n/4;i++){AudioObjectID d=ids[i];CFStringRef nm=0;UInt32 sz=8;AudioObjectPropertyAddress na={kAudioObjectPropertyName,kAudioObjectPropertyScopeGlobal,0};AudioObjectGetPropertyData(d,&na,0,0,&sz,&nm);
  char b[128]="?";if(nm)CFStringGetCString(nm,b,128,kCFStringEncodingUTF8);
  Float64 r=0;sz=8;AudioObjectPropertyAddress ra={kAudioDevicePropertyNominalSampleRate,kAudioObjectPropertyScopeGlobal,0};AudioObjectGetPropertyData(d,&ra,0,0,&sz,&r);
  for(int o=0;o<2;o++){AudioObjectPropertyScope sc=o?kAudioObjectPropertyScopeOutput:kAudioObjectPropertyScopeInput;
   AudioObjectPropertyAddress sa={kAudioDevicePropertyStreams,sc,0};UInt32 ss=0;AudioObjectGetPropertyDataSize(d,&sa,0,0,&ss);if(!ss)continue;
   printf("%-28s %-3s rate %6.0f buffer %4u safety %4u latency %4u\n",b,o?"out":"in",r,u32(d,kAudioDevicePropertyBufferFrameSize,sc),u32(d,kAudioDevicePropertySafetyOffset,sc),u32(d,kAudioDevicePropertyLatency,sc));}}}
