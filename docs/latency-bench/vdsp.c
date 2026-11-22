#include <Accelerate/Accelerate.h>
#include <stdio.h>
#include <stdlib.h>
#include <math.h>
#include <time.h>
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e9+t.tv_nsec;}
#define LIM 1000.0f
__attribute__((noinline)) void ours(float*d,const float*s,float g,int n){
  for(int i=0;i<n;i++){float x=s[i];x=isfinite(x)?fminf(fmaxf(x,-LIM),LIM):0.0f;d[i]+=x*g;}}
__attribute__((noinline)) void ours_noclean(float*d,const float*s,float g,int n){
  for(int i=0;i<n;i++)d[i]+=s[i]*g;}
__attribute__((noinline)) void v_sma(float*d,const float*s,float g,int n){vDSP_vsma(s,1,&g,d,1,d,1,(vDSP_Length)n);}
__attribute__((noinline)) void v_clean(float*d,const float*s,float g,int n){
  /* vDSP has no is-finite select: clip then scale-add is the closest (NaN not handled) */
  static float tmp[1024];float lo=-LIM,hi=LIM;vDSP_vclip(s,1,&lo,&hi,tmp,1,(vDSP_Length)n);vDSP_vsma(tmp,1,&g,d,1,d,1,(vDSP_Length)n);}
typedef void(*fn)(float*,const float*,float,int);
static double bench(fn f,int n){float*d=calloc(n,4),*s=malloc(n*4);for(int i=0;i<n;i++)s[i]=((i*7)%101)/101.0f-0.5f;
  for(int i=0;i<20000;i++)f(d,s,0.5f,n);double best=1e30;
  for(int r=0;r<5;r++){double t=now();for(int i=0;i<200000;i++){f(d,s,0.5f,n);if(i%64==0)for(int k=0;k<n;k++)d[k]=0;}
    double e=(now()-t)/200000;if(e<best)best=e;}
  return best;}
int main(){int ns[]={64,128,256};printf("frames   ours(clean)  ours(no clean)  vDSP_vsma  vclip+vsma   [ns per channel]\n");
  for(int k=0;k<3;k++){int n=ns[k];printf("%5d  %10.1f  %12.1f  %10.1f  %10.1f\n",n,bench(ours,n),bench(ours_noclean,n),bench(v_sma,n),bench(v_clean,n));}}
