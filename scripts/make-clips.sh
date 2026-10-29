#!/bin/sh
# Generates low-latency-style test streams (P-frames only, no B-frames) for
# tools/decodebench until real NVENC output exists. Noisy content = hard case.
set -eu
out="${1:-bench/clips}"
secs="${SECS:-5}"
mkdir -p "$out"
for res in 2560x1440 3840x2160; do
  case $res in 2560x1440) br=80M; av1br=80M ;; *) br=150M; av1br=100M ;; esac  # SVT-AV1 caps at 100M
  src="testsrc2=s=$res:r=120:d=$secs,noise=alls=12:allf=t,format=yuv420p"
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "$src" \
    -c:v libx265 -preset ultrafast -tune zerolatency -b:v $br -maxrate $br -bufsize $br \
    -x265-params "keyint=600:bframes=0:aud=1:repeat-headers=1:log-level=error" \
    -f hevc "$out/hevc-$res.h265"
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "$src" \
    -c:v libsvtav1 -preset 10 -g 600 -b:v $av1br -svtav1-params "pred-struct=1:rc=2" \
    -f ivf "$out/av1-$res.ivf"
  echo "made $res"
done
ls -la "$out"
