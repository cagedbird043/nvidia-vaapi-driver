#!/usr/bin/env python3
"""Hardware encode regression and C-driver comparison (no system driver changes)."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def probe_attributes(device):
    import ctypes as c
    va = c.CDLL('libva.so.2')
    drm = c.CDLL('libva-drm.so.2')
    drm.vaGetDisplayDRM.argtypes = [c.c_int]
    drm.vaGetDisplayDRM.restype = c.c_void_p
    class Attribute(c.Structure):
        _fields_ = [('type', c.c_int), ('value', c.c_uint)]
    va.vaInitialize.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_int)]
    va.vaQueryConfigProfiles.argtypes = [c.c_void_p, c.POINTER(c.c_int), c.POINTER(c.c_int)]
    va.vaQueryConfigEntrypoints.argtypes = [c.c_void_p, c.c_int, c.POINTER(c.c_int), c.POINTER(c.c_int)]
    va.vaGetConfigAttributes.argtypes = [c.c_void_p, c.c_int, c.c_int, c.POINTER(Attribute), c.c_int]
    va.vaTerminate.argtypes = [c.c_void_p]
    fd = os.open(device, os.O_RDWR)
    display = drm.vaGetDisplayDRM(fd)
    major, minor = c.c_int(), c.c_int()
    if va.vaInitialize(display, c.byref(major), c.byref(minor)):
        raise RuntimeError('vaInitialize failed')
    profiles, count = (c.c_int * 128)(), c.c_int()
    if va.vaQueryConfigProfiles(display, profiles, c.byref(count)):
        raise RuntimeError('vaQueryConfigProfiles failed')
    result = {}
    for profile in profiles[:count.value]:
        entries, n = (c.c_int * 32)(), c.c_int()
        status = va.vaQueryConfigEntrypoints(display, profile, entries, c.byref(n))
        if status == 12:  # VA_STATUS_ERROR_UNSUPPORTED_PROFILE, also skipped by vainfo.
            continue
        if status:
            raise RuntimeError(f'vaQueryConfigEntrypoints({profile}) failed: {status}')
        if 6 not in entries[:n.value]:
            continue
        attrs = (Attribute * 57)(*(Attribute(t, 0) for t in range(57)))
        if va.vaGetConfigAttributes(display, profile, 6, attrs, len(attrs)):
            raise RuntimeError('vaGetConfigAttributes failed')
        result[profile] = {a.type: a.value for a in attrs}
    va.vaTerminate(display)
    os.close(fd)
    return result


DIRECT_CLIENT = r'''
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_enc_h264.h>
#include <va/va_enc_hevc.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#define CHECK(call) do { VAStatus status=(call); if(status) {fprintf(stderr,"%s: %s\n",#call,vaErrorStr(status));exit(1);} } while(0)
int main(int argc,char **argv) {
    if(argc!=10) return 2;
    int fd=open(argv[1],O_RDWR), hevc=atoi(argv[2]), w=atoi(argv[3]), h=atoi(argv[4]);
    int mode=atoi(argv[5]), upload=atoi(argv[8]), bits=atoi(argv[9]);
    VADisplay d=vaGetDisplayDRM(fd);int major,minor;CHECK(vaInitialize(d,&major,&minor));
    VAProfile profile=hevc?VAProfileHEVCMain444_10:VAProfileH264High;
    unsigned rt=hevc?VA_RT_FORMAT_YUV444_10:VA_RT_FORMAT_YUV444;
    unsigned fourcc=hevc?VA_FOURCC_Q416:VA_FOURCC_444P;
    VAConfigAttrib attrs[2]={{VAConfigAttribRTFormat,rt},{VAConfigAttribRateControl,mode==0?VA_RC_CQP:mode==1?VA_RC_CBR:VA_RC_VBR}};
    VAConfigID config;CHECK(vaCreateConfig(d,profile,VAEntrypointEncSlice,attrs,2,&config));
    VASurfaceAttrib sa={.type=VASurfaceAttribPixelFormat,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypeInteger,.value.i=fourcc}};
    VASurfaceID surface;CHECK(vaCreateSurfaces(d,rt,w,h,&surface,1,&sa,1));
    VAContextID ctx;CHECK(vaCreateContext(d,config,w,h,VA_PROGRESSIVE,&surface,1,&ctx));
    VAImageFormat *formats=calloc(vaMaxNumImageFormats(d),sizeof(*formats));int nf=0;CHECK(vaQueryImageFormats(d,formats,&nf));
    VAImageFormat format={0};for(int i=0;i<nf;i++)if(formats[i].fourcc==fourcc)format=formats[i];free(formats);
    if(!format.fourcc){fprintf(stderr,"required image format absent\n");return 1;}
    VAImage image={0};if(upload)CHECK(vaCreateImage(d,&format,w,h,&image));
    VABufferID coded;CHECK(vaCreateBuffer(d,ctx,VAEncCodedBufferType,w*h*8,1,NULL,&coded));
    VABufferID seq,rc,fr;
    if(hevc){
        VAEncSequenceParameterBufferHEVC s={0};s.general_profile_idc=4;s.general_level_idc=153;s.intra_period=60;s.intra_idr_period=60;s.ip_period=1;s.bits_per_second=bits;s.pic_width_in_luma_samples=w;s.pic_height_in_luma_samples=h;s.seq_fields.bits.chroma_format_idc=3;s.seq_fields.bits.bit_depth_luma_minus8=2;s.seq_fields.bits.bit_depth_chroma_minus8=2;s.log2_min_luma_coding_block_size_minus3=1;s.log2_diff_max_min_luma_coding_block_size=1;s.log2_diff_max_min_transform_block_size=3;
        CHECK(vaCreateBuffer(d,ctx,VAEncSequenceParameterBufferType,sizeof(s),1,&s,&seq));
    }else{
        VAEncSequenceParameterBufferH264 s={0};s.level_idc=51;s.intra_period=60;s.intra_idr_period=60;s.ip_period=1;s.bits_per_second=bits;s.max_num_ref_frames=1;s.picture_width_in_mbs=(w+15)/16;s.picture_height_in_mbs=(h+15)/16;s.seq_fields.bits.chroma_format_idc=3;s.seq_fields.bits.frame_mbs_only_flag=1;s.frame_cropping_flag=1;s.frame_crop_right_offset=((w+15)/16)*16-w;s.frame_crop_bottom_offset=((h+15)/16)*16-h;
        CHECK(vaCreateBuffer(d,ctx,VAEncSequenceParameterBufferType,sizeof(s),1,&s,&seq));
    }
    _Alignas(VAEncMiscParameterRateControl) unsigned char rate[sizeof(VAEncMiscParameterBuffer)+sizeof(VAEncMiscParameterRateControl)]={0};
    VAEncMiscParameterBuffer *m=(void*)rate;m->type=VAEncMiscParameterTypeRateControl;
    VAEncMiscParameterRateControl *r=(void*)m->data;r->bits_per_second=bits;r->target_percentage=100;r->initial_qp=26;
    CHECK(vaCreateBuffer(d,ctx,VAEncMiscParameterBufferType,sizeof(rate),1,rate,&rc));
    _Alignas(VAEncMiscParameterFrameRate) unsigned char fps[sizeof(VAEncMiscParameterBuffer)+sizeof(VAEncMiscParameterFrameRate)]={0};
    m=(void*)fps;m->type=VAEncMiscParameterTypeFrameRate;((VAEncMiscParameterFrameRate*)m->data)->framerate=30;
    CHECK(vaCreateBuffer(d,ctx,VAEncMiscParameterBufferType,sizeof(fps),1,fps,&fr));
    FILE *in=fopen(argv[6],"rb"),*out=fopen(argv[7],"wb");if(!in||!out)return 1;
    int bps=hevc?2:1;size_t plane=(size_t)w*h*bps;unsigned char *pixels=malloc(plane*3);
    for(int n=0;n<120;n++){
        if(fread(pixels,plane*3,1,in)!=1)return 1;
        if(!upload)CHECK(vaDeriveImage(d,surface,&image));
        void *map;CHECK(vaMapBuffer(d,image.buf,&map));
        for(int p=0;p<3;p++)for(int y=0;y<h;y++)memcpy((char*)map+image.offsets[p]+y*image.pitches[p],pixels+p*plane+(size_t)y*w*bps,w*bps);
        CHECK(vaUnmapBuffer(d,image.buf));if(upload)CHECK(vaPutImage(d,surface,image.image_id,0,0,w,h,0,0,w,h));else CHECK(vaDestroyImage(d,image.image_id));
        VABufferID pic,slice;
        if(hevc){
            VAEncPictureParameterBufferHEVC p={0};p.coded_buf=coded;p.pic_init_qp=26;p.pic_fields.bits.idr_pic_flag=n==0||n==45||n==105;p.pic_fields.bits.coding_type=p.pic_fields.bits.idr_pic_flag?1:2;p.pic_fields.bits.reference_pic_flag=1;
            CHECK(vaCreateBuffer(d,ctx,VAEncPictureParameterBufferType,sizeof(p),1,&p,&pic));
            VAEncSliceParameterBufferHEVC s={0};s.num_ctu_in_slice=((w+31)/32)*((h+31)/32);s.slice_type=p.pic_fields.bits.idr_pic_flag?2:1;
            CHECK(vaCreateBuffer(d,ctx,VAEncSliceParameterBufferType,sizeof(s),1,&s,&slice));
        }else{
            VAEncPictureParameterBufferH264 p={0};p.coded_buf=coded;p.pic_init_qp=26;p.frame_num=n%60;p.pic_fields.bits.idr_pic_flag=n==0||n==45||n==105;p.pic_fields.bits.reference_pic_flag=1;
            CHECK(vaCreateBuffer(d,ctx,VAEncPictureParameterBufferType,sizeof(p),1,&p,&pic));
            VAEncSliceParameterBufferH264 s={0};s.num_macroblocks=((w+15)/16)*((h+15)/16);s.slice_type=p.pic_fields.bits.idr_pic_flag?2:0;
            CHECK(vaCreateBuffer(d,ctx,VAEncSliceParameterBufferType,sizeof(s),1,&s,&slice));
        }
        VABufferID buffers[]={seq,rc,fr,pic,slice};CHECK(vaBeginPicture(d,ctx,surface));CHECK(vaRenderPicture(d,ctx,buffers,5));CHECK(vaEndPicture(d,ctx));CHECK(vaSyncSurface(d,surface));
        VACodedBufferSegment *segment;CHECK(vaMapBuffer(d,coded,(void**)&segment));for(;segment;segment=segment->next)if(fwrite(segment->buf,segment->size,1,out)!=1)return 1;CHECK(vaUnmapBuffer(d,coded));CHECK(vaDestroyBuffer(d,pic));CHECK(vaDestroyBuffer(d,slice));
    }
    free(pixels);fclose(in);fclose(out);if(upload)CHECK(vaDestroyImage(d,image.image_id));
    CHECK(vaDestroyBuffer(d,seq));CHECK(vaDestroyBuffer(d,rc));CHECK(vaDestroyBuffer(d,fr));CHECK(vaDestroyBuffer(d,coded));CHECK(vaDestroyContext(d,ctx));CHECK(vaDestroySurfaces(d,&surface,1));CHECK(vaDestroyConfig(d,config));CHECK(vaTerminate(d));close(fd);return 0;
}
'''


def direct_chroma(args, env, directory):
    source = args.output / 'encode-client.c'
    exe = args.output / 'encode-client'
    source.write_text(DIRECT_CLIENT)
    run(['cc', '-O2', '-Wall', '-Wextra', str(source), '-o', str(exe), '-lva', '-lva-drm'])
    rows = []
    for hevc, fmt in [(0, 'yuv444p'), (1, 'yuv444p16le')]:
        for size in SIZES:
            raw = args.output / f'{fmt}-{size}.raw'
            if not raw.exists():
                run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', f'testsrc2=size={size}:rate=30', '-vf', f'format={fmt}', '-frames:v', '120', '-f', 'rawvideo', '-y', str(raw)])
            for mode, name in enumerate(['CQP','CBR','VBR']):
                for upload in [0, 1]:
                    path = directory / f'direct-{hevc}-{size}-{name}-{upload}.hevc' if hevc else directory / f'direct-{hevc}-{size}-{name}-{upload}.h264'
                    row = dict(codec='hevc' if hevc else 'h264', format=fmt, size=size, mode=name, upload='put' if upload else 'derive')
                    try:
                        w, h = size.split('x')
                        run([str(exe), args.device, str(hevc), w, h, str(mode), str(raw), str(path), str(upload), str(args.bitrate)], env)
                        row.update(inspect(args, path, fmt, size, 120), ok=True)
                        row['rate_within_10pct'] = abs(row['bitrate']/args.bitrate-1)<=.1 if mode else None
                        row['ok'] = (mode==0 or row['rate_within_10pct']) and 45 in row['keyframes']
                    except RuntimeError as error:
                        row.update(ok=False,error=str(error))
                    rows.append(row)
    return rows


if len(sys.argv) == 3 and sys.argv[1] == '--probe':
    print(json.dumps(probe_attributes(sys.argv[2]), sort_keys=True))
    raise SystemExit(0)

# FFmpeg cannot negotiate H.264 4:4:4 or HEVC Q416 through hwupload; direct_chroma covers both.
VARIANTS = [
    ('h264', 'constrained_baseline', 'nv12'), ('h264', 'main', 'nv12'),
    ('h264', 'high', 'nv12'),
    ('hevc', 'main', 'nv12'), ('hevc', 'main10', 'p010le'),
    ('hevc', 'rext', 'yuv444p'),
    ('av1', 'main', 'nv12'), ('av1', 'main', 'p010le'),
]
SIZES = ['1920x1080', '1280x720', '642x362']


def run(cmd, env=None):
    p = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if p.returncode:
        raise RuntimeError(f"exit {p.returncode}: {' '.join(map(str, cmd))}\n{p.stderr}")
    return p


def environment(driver, library):
    e = os.environ.copy()
    e['LIBVA_DRIVER_NAME'] = driver
    if library:
        e['LIBVA_DRIVERS_PATH'] = str(Path(library).resolve())
    else:
        e.pop('LIBVA_DRIVERS_PATH', None)
    return e


def command(args, codec, profile, fmt, size, mode, path, frames=120, bounds=None):
    cmd = ['ffmpeg', '-hide_banner', '-v', 'warning', '-init_hw_device',
           f'vaapi=va:{args.device}', '-filter_hw_device', 'va', '-f', 'lavfi',
           '-i', f'testsrc2=size={size}:rate=30', '-vf', f'format={fmt},hwupload',
           '-frames:v', str(frames), '-c:v', f'{codec}_vaapi', '-profile:v', profile,
           '-rc_mode', mode, '-g', '60', '-bf', '0', '-force_key_frames', '1.5', '-y']
    if mode == 'CQP':
        cmd += ['-qp' if codec != 'av1' else '-global_quality', '26' if codec != 'av1' else '100']
    else:
        cmd += ['-b:v', str(args.bitrate), '-maxrate', str(args.bitrate), '-bufsize', str(args.bitrate * 2)]
    if bounds:
        cmd += ['-qmin', str(bounds[0]), '-qmax', str(bounds[1])]
    return cmd + [str(path)]


def inspect(args, path, fmt, size, frames):
    run(['ffmpeg', '-v', 'error', '-xerror', '-i', str(path), '-f', 'null', '-'])
    probe = json.loads(run(['ffprobe', '-v', 'error', '-count_frames', '-show_streams',
                            '-show_packets', '-of', 'json', str(path)]).stdout)
    stream = probe['streams'][0]
    count = int(stream['nb_read_frames'])
    if count != frames:
        raise RuntimeError(f'frame count {count}, expected {frames}')
    if f"{stream['width']}x{stream['height']}" != size:
        raise RuntimeError(f'unexpected visible size: {stream["width"]}x{stream["height"]}')
    expected = {'nv12':'yuv420p','p010le':'yuv420p10le','yuv444p':'yuv444p','yuv444p16le':'yuv444p10le'}[fmt]
    if stream['pix_fmt'] != expected:
        raise RuntimeError(f'pixel format fallback: expected {expected}, got {stream["pix_fmt"]}')
    bitrate = sum(int(p['size']) for p in probe['packets']) * 8 * 30 / frames
    # Raw inputs make PSNR independent of driver color tags and filter scheduling.
    quality_frames = min(frames, 120)
    reference = args.output / f'psnr-{fmt}-{size}.raw'
    if not reference.exists():
        run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', f'testsrc2=size={size}:rate=30', '-vf', f'format={fmt},format={expected}', '-frames:v', str(quality_frames), '-f', 'rawvideo', '-y', str(reference)])
    with tempfile.TemporaryDirectory(prefix='rustnv-psnr-') as temp:
        decoded = Path(temp) / 'decoded.raw'
        run(['ffmpeg', '-v', 'error', '-i', str(path), '-frames:v', str(quality_frames), '-pix_fmt', expected, '-f', 'rawvideo', '-y', str(decoded)])
        raw_args = ['-f', 'rawvideo', '-pixel_format', expected, '-video_size', size, '-framerate', '30']
        psnr = run(['ffmpeg', '-v', 'info', *raw_args, '-i', str(decoded), *raw_args, '-i', str(reference), '-lavfi', 'psnr', '-frames:v', str(quality_frames), '-f', 'null', '-'])
    match = re.findall(r'average:([\d.]+|inf)', psnr.stderr)
    if not match:
        raise RuntimeError('missing PSNR result')
    quality = float(match[-1])
    if not quality > 20:
        raise RuntimeError(f'PSNR too low: {quality}')
    keyframes = [i for i, p in enumerate(probe['packets']) if 'K' in p.get('flags', '')]
    return dict(frames=count, bitrate=round(bitrate), psnr=quality, keyframes=keyframes,
                pixel_format=stream['pix_fmt'], width=stream['width'], height=stream['height'])


def capabilities(args, env):
    text = run(['vainfo', '--display', 'drm', '--device', args.device, '-a'], env).stdout
    # Keep every reported encoder attribute, including continuation lines.
    blocks = [b for b in text.split('\n\n') if '/VAEntrypointEncSlice' in b]
    return '\n\n'.join(sorted(blocks))


def quantizers(args, env, directory):
    rows = []
    for codec, profile, fmt in [VARIANTS[i] for i in (2, 3, 6)]:
        for mode in ['CBR', 'VBR']:
            q = 35
            path = directory / f'{codec}-{mode}-fixed-qp.mkv'
            run(command(args, codec, profile, fmt, '1280x720', mode, path,
                        bounds=(35, 35)), env)
            trace = run(['ffmpeg', '-v', 'debug', '-i', str(path), '-c:v', 'copy',
                         '-bsf:v', 'trace_headers', '-f', 'null', '-']).stderr
            (directory / f'{codec}-{mode}-qp-trace.log').write_text(trace)
            if codec == 'av1':
                values = [int(n) for n in re.findall(r'base_q_idx\s+[^=]+\s=\s(\d+)', trace)]
            else:
                base_name = 'pic_init_qp_minus26' if codec == 'h264' else 'init_qp_minus26'
                base = re.findall(base_name + r'\s+[^=]+\s=\s(-?\d+)', trace)
                delta = re.findall(r'slice_qp_delta\s+[^=]+\s=\s(-?\d+)', trace)
                values = [26 + int(base[-1]) + int(d) for d in delta] if base else []
            if len(values) < 120 or any(v != q for v in values):
                raise RuntimeError(f'{codec}/{mode} quantizer bounds ineffective: {sorted(set(values))}, {len(values)} slices')
            rows.append(dict(codec=codec, mode=mode, min_qp=min(values), max_qp=max(values), frames=len(values)))
    return rows


def stability(args, env, directory):
    path = directory / 'stability.mkv'
    cmd = command(args, 'h264', 'high', 'nv12', '1920x1080', 'CQP', path, 3000)
    cmd[1:1] = ['-progress', 'pipe:1', '-stats_period', '0.1']
    with (directory / 'stability.log').open('w') as log:
        p = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=log, text=True)
        samples = []
        for line in p.stdout:
            if line.startswith('frame='):
                frame = int(line.split('=')[1])
                try:
                    status = Path(f'/proc/{p.pid}/status').read_text()
                    rss = int(re.search(r'VmRSS:\s+(\d+)', status)[1])
                    samples.append((frame, rss))
                except (FileNotFoundError, TypeError):
                    pass
        if p.wait():
            raise RuntimeError('stability encode failed; see stability.log')
    warm = next((s for s in samples if s[0] >= 300), None)
    end = next((s for s in reversed(samples) if s[0] < 3000), None)
    if warm is None or end is None:
        raise RuntimeError('missing stability RSS samples')
    inspect(args, path, 'nv12', '1920x1080', 3000)
    growth = end[1] - warm[1]
    if growth > 16 * 1024:
        raise RuntimeError(f'RSS grew by {growth} KiB')
    return dict(frames=3000, warmup=warm, end=end, growth_kib=growth, samples=samples)


def transcode(args, env, directory):
    source = directory / 'transcode-source.mkv'
    run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=1280x720:rate=30',
         '-frames:v', '120', '-c:v', 'libx264', '-bf', '0', '-y', str(source)])
    dest = directory / 'transcode.mkv'
    run(['ffmpeg', '-v', 'warning', '-hwaccel', 'vaapi', '-hwaccel_device', args.device,
         '-hwaccel_output_format', 'vaapi', '-i', str(source), '-c:v', 'h264_vaapi',
         '-rc_mode', 'CQP', '-qp', '26', '-y', str(dest)], env)
    return inspect(args, dest, 'nv12', '1280x720', 120)


def build_tools(directory):
    tests = Path(__file__).resolve().parent
    flags = lambda *libs: subprocess.run(['pkg-config', '--cflags', '--libs', *libs], stdout=subprocess.PIPE, text=True, check=True).stdout.split()
    tools = {}
    for name, libs, extra in [('dmabuf_import', ['libva', 'libva-drm', 'libdrm'], ['-ldl']),
                              ('dmabuf_export', ['libva', 'libavcodec', 'libavformat', 'libavutil'], []),
                              ('dmabuf_write', ['egl', 'glesv2', 'gbm', 'libva', 'libavcodec', 'libavutil'], [])]:
        tools[name] = directory / name
        run(['gcc', '-O1', '-o', str(tools[name]), str(tests / f'{name}.c'), *flags(*libs), *extra])
    return tools


def bgra_reference(directory, w, h, frames):
    # Same pattern as dmabuf_import.c; converted with swscale BT.709 limited range.
    raw = directory / 'bgra.raw'
    with open(raw, 'wb') as f:
        for n in range(frames):
            b = bytearray(w * h * 4)
            for j in range(h):
                for i in range(w):
                    o = (j * w + i) * 4
                    b[o] = (i * 2 + n * 3 + (j & 7) * 5) & 255
                    b[o + 1] = (j * 2 + n * 5 + (i & 3) * 9) & 255
                    b[o + 2] = (i + j + n * 7) & 255
                    b[o + 3] = 255
            f.write(b)
    ref = directory / 'bgra-ref.mkv'
    run(['ffmpeg', '-v', 'error', '-f', 'rawvideo', '-pix_fmt', 'bgra', '-s', f'{w}x{h}', '-i', str(raw),
         '-vf', 'scale=out_color_matrix=bt709:out_range=tv,format=yuv420p', '-c:v', 'ffv1', '-y', str(ref)])
    return ref


def zero_copy(args, env, directory):
    """Decode-surface and external dma-buf inputs against the same content uploaded with vaPutImage."""
    tools = build_tools(directory)
    node, rows = args.device, []

    def check(name, fn):
        try:
            rows.append(dict(case=name, ok=True, **(fn() or {})))
        except RuntimeError as error:
            rows.append(dict(case=name, ok=False, error=str(error)[-300:]))
        print('zero-copy', json.dumps(rows[-1]), flush=True)

    def encode(codec, fmt, source, w, h, frames=12):
        out = directory / f'zc-{codec}-{fmt}-{source}-{w}x{h}.bin'
        run([str(tools['dmabuf_import']), node, codec, fmt, source, str(w), str(h), str(frames), str(out)], env)
        return out.read_bytes()

    def same_as_put(codec, fmt, source, w, h):
        def fn():
            if encode(codec, fmt, source, w, h) != encode(codec, fmt, 'put', w, h):
                raise RuntimeError('bitstream differs from vaPutImage upload')
        return fn
    for codec, fmt in [('h264', 'nv12'), ('hevc', 'nv12'), ('hevc', 'p010'), ('h264', '444p'), ('hevc', '444p'), ('hevc', 'q416')]:
        for source in ['linear', 'bl']:
            for w, h in [(1280, 720), (656, 368)]:
                check(f'import-{codec}-{fmt}-{source}-{w}x{h}', same_as_put(codec, fmt, source, w, h))

    def bgra():
        w, h, frames = 640, 360, 6
        ref = bgra_reference(directory, w, h, frames)
        streams = [encode('h264', 'bgra', source, w, h, frames) for source in ('linear', 'bl')]
        if streams[0] != streams[1]:
            raise RuntimeError('linear and block-linear BGRA differ')
        raw = directory / 'zc-h264-bgra-linear-640x360.bin'
        text = run(['ffmpeg', '-v', 'info', '-i', str(raw), '-i', str(ref), '-lavfi', '[0:v]format=yuv420p[a];[1:v]format=yuv420p[b];[a][b]psnr',
                    '-f', 'null', '-'], None).stderr
        psnr = float(re.findall(r'average:([\d.]+)', text)[-1])
        if psnr < 30:
            raise RuntimeError(f'BGRA PSNR {psnr}')
        return dict(psnr=psnr)
    check('import-bgra-linear-bl-psnr', bgra)

    source = directory / 'zc-source.mp4'
    run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', 'testsrc2=size=1280x720:rate=30', '-frames:v', '150',
         '-pix_fmt', 'yuv420p', '-c:v', 'libx264', '-bf', '0', '-g', '60', '-y', str(source)])

    def export_reimport():
        outs = []
        for mode in ('direct', 'reimport'):
            out = directory / f'zc-export-{mode}.h264'
            run([str(tools['dmabuf_export']), node, str(source), mode, str(out)], env)
            outs.append(out.read_bytes())
        if outs[0] != outs[1]:
            raise RuntimeError('export -> DRM_PRIME_2 import differs from direct transcode')
    check('export-reimport-bytes', export_reimport)

    def decode_surface(encoder, options, size, pix_fmt):
        """A decode surface straight into the encoder (copy engine) against the same decode uploaded with hwupload."""
        def fn():
            w, h = size
            clip = directory / f'zc-{pix_fmt}-{w}x{h}.mkv'
            if not clip.exists():
                run(['ffmpeg', '-v', 'error', '-f', 'lavfi', '-i', f'testsrc2=size={w}x{h}:rate=30', '-frames:v', '45',
                     '-pix_fmt', pix_fmt, '-c:v', 'libx265', '-x265-params', 'bframes=3:keyint=30:log-level=error', '-y', str(clip)])
            upload = {'yuv420p10le': 'p010le'}.get(pix_fmt, pix_fmt)
            outs = []
            for mode in ('surface', 'upload'):
                out = directory / f'zc-{pix_fmt}-{encoder}-{w}x{h}-{mode}.bin'
                head = ['ffmpeg', '-v', 'error', '-y']
                if mode == 'surface':
                    head += ['-hwaccel', 'vaapi', '-hwaccel_device', node, '-hwaccel_output_format', 'vaapi', '-i', str(clip), '-vf', 'null']
                else:
                    head += ['-vaapi_device', node, '-i', str(clip), '-vf', f'format={upload},hwupload']
                run(head + ['-frames:v', '45', '-c:v', encoder, *options, '-g', '30', '-bf', '0', '-f', 'rawvideo', str(out)], env)
                outs.append(out.read_bytes())
            if not outs[0] or outs[0] != outs[1]:
                raise RuntimeError(f'decode-surface encode differs from {upload} upload')
            return dict(bytes=len(outs[0]))
        return fn
    # FFmpeg's VA-API decoder has no pixel format for Q416 (10-bit 4:4:4 decode already fails at vaBeginPicture),
    # so that decode surface's layout is covered by the Q416 imports above.
    for encoder, options, pix_fmt in [('hevc_vaapi', ['-profile:v', 'main10', '-qp', '28'], 'yuv420p10le'), ('av1_vaapi', ['-qp', '100'], 'yuv420p10le'),
                                      ('hevc_vaapi', ['-profile:v', 'rext', '-qp', '28'], 'yuv444p')]:
        for size in [(1280, 720), (656, 368)]:
            check(f'decode-surface-{pix_fmt}-{encoder}-{size[0]}x{size[1]}', decode_surface(encoder, options, size, pix_fmt))

    def cycles(codec, fmt, src):
        def fn():
            text = run([str(tools['dmabuf_import']), node, codec, fmt, src, '640', '360', '0', os.devnull, '1000'], env).stdout
            m = re.search(r'rss_kb (\d+) -> (\d+) fds (\d+) -> (\d+)', text)
            r0, r1, f0, f1 = map(int, m.groups())
            if f1 > f0 or r1 > r0 + 512:
                raise RuntimeError(text)
            return dict(rss_kb=[r0, r1], fds=[f0, f1])
        return fn
    for codec, fmt, src in [('h264', 'nv12', 'linear'), ('h264', 'nv12', 'bl'), ('hevc', 'p010', 'bl'), ('h264', 'bgra', 'linear')]:
        check(f'import-cycles-{codec}-{fmt}-{src}', cycles(codec, fmt, src))

    def written(encoder, fmt):
        """Sunshine's path: one surface exported writable once, EGL draws each frame into it, the encoder reads it back."""
        def fn():
            text = run([str(tools['dmabuf_write']), node, encoder, fmt, '120'], {**env, 'DMABUF_WRITE_CYCLES': '1000'}).stdout
            return dict(result=text.strip().splitlines())
        return fn
    for encoder, fmt in [('h264_vaapi', 'NV12'), ('hevc_vaapi', 'NV12'), ('hevc_vaapi', 'P010'), ('av1_vaapi', 'NV12'), ('av1_vaapi', 'P010')]:
        check(f'export-writable-{encoder}-{fmt}', written(encoder, fmt))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--driver', default=os.environ.get('LIBVA_DRIVER_NAME', 'rustnv'))
    parser.add_argument('--library', default=os.environ.get('LIBVA_DRIVERS_PATH', 'target/release'))
    parser.add_argument('--reference-driver', default='nvidia')
    parser.add_argument('--reference-library', default='')
    parser.add_argument('--device', default='/dev/dri/renderD129')
    parser.add_argument('--output', type=Path, default=Path('/tmp/rustnv-encode-regression'))
    parser.add_argument('--bitrate', type=int, default=4_000_000)
    parser.add_argument('--quick', action='store_true', help='one size and profile per codec')
    parser.add_argument('--skip-stability', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = {}
    variants = [VARIANTS[i] for i in (2, 3, 6)] if args.quick else VARIANTS
    sizes = ['642x362'] if args.quick else SIZES
    for label, driver, library in [('reference', args.reference_driver, args.reference_library),
                                    ('candidate', args.driver, args.library)]:
        env = environment(driver, library)
        directory = args.output / label
        directory.mkdir(exist_ok=True)
        (directory / 'vainfo.txt').write_text(capabilities(args, env))
        attributes = json.loads(run([sys.executable, __file__, '--probe', args.device], env).stdout)
        (directory / 'attributes.json').write_text(json.dumps(attributes, indent=2, sort_keys=True))
        rows = []
        for codec, profile, fmt in variants:
            for size in sizes:
                for mode in ['CQP', 'CBR', 'VBR']:
                    name = f'{codec}-{profile}-{fmt}-{size}-{mode}'
                    path = directory / f'{name}.mkv'
                    row = dict(case=name)
                    try:
                        run(command(args, codec, profile, fmt, size, mode, path), env)
                        row.update(inspect(args, path, fmt, size, 120))
                        if 0 not in row['keyframes'] or 45 not in row['keyframes']:
                            raise RuntimeError(f'missing forced keyframe at frame 45: {row["keyframes"]}')
                        row['rate_within_10pct'] = abs(row['bitrate'] / args.bitrate - 1) <= .1 if mode != 'CQP' else None
                        row['ok'] = mode == 'CQP' or row['rate_within_10pct']
                    except RuntimeError as error:
                        row.update(ok=False, error=str(error))
                    rows.append(row)
                    print(label, name, json.dumps(row), flush=True)
                    results[label] = rows
                    (args.output / 'results.json').write_text(json.dumps(results, indent=2))
        for name, check in [('direct', direct_chroma), ('quantizers', quantizers), ('transcode', transcode)] + ([] if args.skip_stability else [('stability', stability)]):
            try:
                results[f'{label}_{name}'] = check(args, env, directory)
            except RuntimeError as error:
                results[f'{label}_{name}_extra_error'] = str(error)
    # Zero-copy inputs only exist in the candidate; the C driver does not run these programs.
    results['candidate_zero_copy'] = zero_copy(args, environment(args.driver, args.library), args.output / 'candidate')
    results['capabilities_equal'] = (args.output / 'reference/vainfo.txt').read_text() == (args.output / 'candidate/vainfo.txt').read_text()
    results['attributes_equal'] = json.loads((args.output / 'reference/attributes.json').read_text()) == json.loads((args.output / 'candidate/attributes.json').read_text())
    (args.output / 'results.json').write_text(json.dumps(results, indent=2))
    # The exit status judges the candidate; the reference driver's own failures stay in the JSON for comparison.
    passed = (results['capabilities_equal'] and results['attributes_equal']
              and all(row['ok'] for label in ('candidate', 'candidate_direct') for row in results.get(label, []))
              and all(row['ok'] for row in results['candidate_zero_copy'])
              and not any(k.startswith('candidate_') and k.endswith('_extra_error') for k in results))
    return 0 if passed else 1


if __name__ == '__main__':
    raise SystemExit(main())
