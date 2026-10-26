// Decodes with VA-API, then encodes H.264 either straight from the decode surface (direct)
// or from a new surface imported from vaExportSurfaceHandle(DRM_PRIME_2) (reimport).
// usage: dmabuf_export <render node> <input> <direct|reimport> <out.h264> [cycles]
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_enc_h264.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define CHECK(call) do { VAStatus status=(call); if(status) {fprintf(stderr,"%s: %s\n",#call,vaErrorStr(status));exit(1);} } while(0)

static enum AVPixelFormat pick(AVCodecContext *c, const enum AVPixelFormat *f) {
    for (; *f != AV_PIX_FMT_NONE; f++) if (*f == AV_PIX_FMT_VAAPI) return *f;
    return AV_PIX_FMT_NONE;
}
static long rss_kb(void){FILE*f=fopen("/proc/self/statm","r");long a,b;if(fscanf(f,"%ld %ld",&a,&b)!=2)b=0;fclose(f);return b*4;}
static int fd_count(void){char cmd[64];int n=0;snprintf(cmd,64,"ls /proc/%d/fd | wc -l",getpid());FILE*f=popen(cmd,"r");if(fscanf(f,"%d",&n)!=1)n=0;pclose(f);return n;}

static VASurfaceID reimport(VADisplay d, VASurfaceID s, int w, int h) {
    VADRMPRIMESurfaceDescriptor desc;
    CHECK(vaExportSurfaceHandle(d, s, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2, VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &desc));
    CHECK(vaSyncSurface(d, s)); // the export queues the copy into the shared backing; sync waits for it
    VASurfaceAttrib at[2] = {
        {.type=VASurfaceAttribMemoryType,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypeInteger,.value.i=VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2}},
        {.type=VASurfaceAttribExternalBufferDescriptor,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value={.type=VAGenericValueTypePointer,.value.p=&desc}}};
    VASurfaceID n;
    CHECK(vaCreateSurfaces(d, VA_RT_FORMAT_YUV420, w, h, &n, 1, at, 2));
    for (uint32_t i = 0; i < desc.num_objects; i++) close(desc.objects[i].fd);
    return n;
}

int main(int argc, char **argv) {
    if (argc < 5) return 2;
    int reimport_mode = !strcmp(argv[3], "reimport"), cycles = argc > 5 ? atoi(argv[5]) : 0;
    AVFormatContext *fmt = NULL;
    if (avformat_open_input(&fmt, argv[2], NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) return 1;
    int idx = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    const AVCodec *codec = avcodec_find_decoder(fmt->streams[idx]->codecpar->codec_id);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    avcodec_parameters_to_context(dec, fmt->streams[idx]->codecpar);
    AVBufferRef *dev = NULL;
    if (av_hwdevice_ctx_create(&dev, AV_HWDEVICE_TYPE_VAAPI, argv[1], NULL, 0) < 0) return 1;
    dec->hw_device_ctx = av_buffer_ref(dev);
    dec->get_format = pick;
    if (avcodec_open2(dec, codec, NULL) < 0) return 1;
    VADisplay d = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)dev->data)->hwctx)->display;
    int w = dec->width, h = dec->height;

    VAConfigAttrib attrs[2] = {{VAConfigAttribRTFormat, VA_RT_FORMAT_YUV420}, {VAConfigAttribRateControl, VA_RC_CQP}};
    VAConfigID config; CHECK(vaCreateConfig(d, VAProfileH264High, VAEntrypointEncSlice, attrs, 2, &config));
    VASurfaceID base; CHECK(vaCreateSurfaces(d, VA_RT_FORMAT_YUV420, w, h, &base, 1, NULL, 0));
    VAContextID ctx; CHECK(vaCreateContext(d, config, w, h, VA_PROGRESSIVE, &base, 1, &ctx));
    VABufferID coded, seq, rc, fr;
    CHECK(vaCreateBuffer(d, ctx, VAEncCodedBufferType, w * h * 8, 1, NULL, &coded));
    VAEncSequenceParameterBufferH264 sp = {0};
    sp.level_idc = 51; sp.intra_period = 60; sp.intra_idr_period = 60; sp.ip_period = 1; sp.bits_per_second = 10000000; sp.max_num_ref_frames = 1;
    sp.picture_width_in_mbs = (w + 15) / 16; sp.picture_height_in_mbs = (h + 15) / 16;
    sp.seq_fields.bits.chroma_format_idc = 1; sp.seq_fields.bits.frame_mbs_only_flag = 1;
    sp.frame_cropping_flag = 1; sp.frame_crop_right_offset = (((w + 15) / 16) * 16 - w) / 2; sp.frame_crop_bottom_offset = (((h + 15) / 16) * 16 - h) / 2;
    CHECK(vaCreateBuffer(d, ctx, VAEncSequenceParameterBufferType, sizeof sp, 1, &sp, &seq));
    _Alignas(VAEncMiscParameterRateControl) unsigned char rate[sizeof(VAEncMiscParameterBuffer) + sizeof(VAEncMiscParameterRateControl)] = {0};
    VAEncMiscParameterBuffer *m = (void *)rate; m->type = VAEncMiscParameterTypeRateControl;
    VAEncMiscParameterRateControl *r = (void *)m->data; r->bits_per_second = 10000000; r->target_percentage = 100; r->initial_qp = 26;
    CHECK(vaCreateBuffer(d, ctx, VAEncMiscParameterBufferType, sizeof rate, 1, rate, &rc));
    _Alignas(VAEncMiscParameterFrameRate) unsigned char fps[sizeof(VAEncMiscParameterBuffer) + sizeof(VAEncMiscParameterFrameRate)] = {0};
    m = (void *)fps; m->type = VAEncMiscParameterTypeFrameRate; ((VAEncMiscParameterFrameRate *)m->data)->framerate = 30;
    CHECK(vaCreateBuffer(d, ctx, VAEncMiscParameterBufferType, sizeof fps, 1, fps, &fr));

    FILE *out = fopen(argv[4], "wb");
    AVPacket *pkt = av_packet_alloc(); AVFrame *frame = av_frame_alloc();
    int n = 0, drained = 0; long rss0 = 0; int fd0 = 0;
    while (!drained) {
        int got = 0;
        if (av_read_frame(fmt, pkt) >= 0) {
            if (pkt->stream_index == idx) { avcodec_send_packet(dec, pkt); got = avcodec_receive_frame(dec, frame) == 0; }
            av_packet_unref(pkt);
        } else { avcodec_send_packet(dec, NULL); got = avcodec_receive_frame(dec, frame) == 0; if (!got) drained = 1; }
        if (!got) continue;
        if (n == 30) { rss0 = rss_kb(); fd0 = fd_count(); }
        VASurfaceID s = (VASurfaceID)(uintptr_t)frame->data[3], t = s;
        if (reimport_mode) t = reimport(d, s, w, h);
        VABufferID pic, slice;
        VAEncPictureParameterBufferH264 p = {0}; p.coded_buf = coded; p.pic_init_qp = 26; p.frame_num = n % 60;
        p.pic_fields.bits.idr_pic_flag = n == 0 || n == 45 || n == 105; p.pic_fields.bits.reference_pic_flag = 1;
        CHECK(vaCreateBuffer(d, ctx, VAEncPictureParameterBufferType, sizeof p, 1, &p, &pic));
        VAEncSliceParameterBufferH264 sl = {0}; sl.num_macroblocks = ((w + 15) / 16) * ((h + 15) / 16); sl.slice_type = p.pic_fields.bits.idr_pic_flag ? 2 : 0;
        CHECK(vaCreateBuffer(d, ctx, VAEncSliceParameterBufferType, sizeof sl, 1, &sl, &slice));
        VABufferID buffers[] = {seq, rc, fr, pic, slice};
        CHECK(vaBeginPicture(d, ctx, t)); CHECK(vaRenderPicture(d, ctx, buffers, 5)); CHECK(vaEndPicture(d, ctx)); CHECK(vaSyncSurface(d, t));
        VACodedBufferSegment *seg; CHECK(vaMapBuffer(d, coded, (void **)&seg));
        for (; seg; seg = seg->next) fwrite(seg->buf, seg->size, 1, out);
        CHECK(vaUnmapBuffer(d, coded)); CHECK(vaDestroyBuffer(d, pic)); CHECK(vaDestroyBuffer(d, slice));
        if (reimport_mode) CHECK(vaDestroySurfaces(d, &t, 1));
        av_frame_unref(frame); n++;
    }
    fclose(out);
    printf("frames=%d rss_kb %ld -> %ld fds %d -> %d\n", n, rss0, rss_kb(), fd0, fd_count());
    (void)cycles;
    return 0;
}
