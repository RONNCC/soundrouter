#import <Metal/Metal.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e6+t.tv_nsec/1e3;}
static int cmp(const void*a,const void*b){double x=*(double*)a,y=*(double*)b;return x<y?-1:x>y;}
int main(){@autoreleasepool{
 NSArray*all=MTLCopyAllDevices();printf("devices: %lu\n",(unsigned long)[all count]);id<MTLDevice> dev=MTLCreateSystemDefaultDevice();if(!dev&&[all count])dev=all[0];printf("device: %s\n",[[dev name] UTF8String]);
 NSError*err=nil;
 id<MTLLibrary> lib=[dev newLibraryWithSource:@"#include <metal_stdlib>\nusing namespace metal;\nkernel void k(device float*d [[buffer(0)]],device const float*s [[buffer(1)]],uint i [[thread_position_in_grid]]){d[i]+=s[i]*0.5f;}" options:nil error:&err];
 if(!lib){printf("lib err %s\n",[[err description] UTF8String]);return 1;}
 id<MTLComputePipelineState> ps=[dev newComputePipelineStateWithFunction:[lib newFunctionWithName:@"k"] error:&err];
 id<MTLCommandQueue> q=[dev newCommandQueue];
 int sizes[]={64,128,256};
 for(int si=0;si<3;si++){int n=sizes[si];
  id<MTLBuffer> d=[dev newBufferWithLength:n*4 options:MTLResourceStorageModeShared],s=[dev newBufferWithLength:n*4 options:MTLResourceStorageModeShared];
  int N=5000;double*t=malloc(N*8);
  for(int it=-200;it<N;it++){double a=now();
   id<MTLCommandBuffer> cb=[q commandBuffer];id<MTLComputeCommandEncoder> e=[cb computeCommandEncoder];
   [e setComputePipelineState:ps];[e setBuffer:d offset:0 atIndex:0];[e setBuffer:s offset:0 atIndex:1];
   [e dispatchThreads:MTLSizeMake(n,1,1) threadsPerThreadgroup:MTLSizeMake(n,1,1)];[e endEncoding];[cb commit];[cb waitUntilCompleted];
   if(it>=0)t[it]=now()-a;}
  qsort(t,N,8,cmp);
  printf("frames %3d: median %.0f us  p99 %.0f us  max %.0f us\n",n,t[N/2],t[N*99/100],t[N-1]);}
 // idle gap case: sleep 50 ms between calls to let the GPU power down
 {int n=128;id<MTLBuffer> d=[dev newBufferWithLength:n*4 options:MTLResourceStorageModeShared],s=[dev newBufferWithLength:n*4 options:MTLResourceStorageModeShared];
  int N=100;double*t=malloc(N*8);
  for(int it=0;it<N;it++){struct timespec ts={0,50*1000*1000};nanosleep(&ts,0);double a=now();
   id<MTLCommandBuffer> cb=[q commandBuffer];id<MTLComputeCommandEncoder> e=[cb computeCommandEncoder];
   [e setComputePipelineState:ps];[e setBuffer:d offset:0 atIndex:0];[e setBuffer:s offset:0 atIndex:1];
   [e dispatchThreads:MTLSizeMake(n,1,1) threadsPerThreadgroup:MTLSizeMake(n,1,1)];[e endEncoding];[cb commit];[cb waitUntilCompleted];t[it]=now()-a;}
  qsort(t,N,8,cmp);printf("128 frames after 50 ms idle: median %.0f us  p99 %.0f us  max %.0f us\n",t[N/2],t[N*99/100],t[N-1]);}
}}
