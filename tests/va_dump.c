// Decodes with libavcodec's VA-API hwaccel and writes every decoded surface exactly as the driver
// hands it back (vaDeriveImage or vaCreateImage + vaGetImage), without any pixel format conversion:
// the visible picture, one plane after another (Y, then CbCr or Cb and Cr), rows tightly packed.
// FFmpeg's hwdownload cannot describe 4:4:4 above 8 bits (the C driver's Q416 has no FFmpeg pixel
// format), so the surfaces are created here and read back in the driver's own format.
// usage: va_dump <render node> <input> <out.raw> <derive|getimage> <fourcc> [frames]
//   fourcc: NV12 P010 P012 444P Q416 (the format of the driver's surfaces for this stream)
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(call) do { VAStatus status_ = (call); if (status_) { fprintf(stderr, "%s: %s\n", #call, vaErrorStr(status_)); exit(1); } } while (0)

static VADisplay display;
static unsigned render_target_format, surface_width, surface_height;
static void free_surface(void *opaque, uint8_t *data) {
    VASurfaceID id = (VASurfaceID)(uintptr_t)data;
    vaDestroySurfaces(display, &id, 1);
}
// Surfaces are created here with the render-target format of the stream: the frames context only
// carries a pixel format FFmpeg accepts (the 8-bit 4:4:4 one above 8 bits) and never allocates.
static AVBufferRef *alloc_surface(void *opaque, size_t size) {
    VASurfaceID id;
    VASurfaceAttrib attribute = {.type = VASurfaceAttribUsageHint, .flags = VA_SURFACE_ATTRIB_SETTABLE,
        .value = {.type = VAGenericValueTypeInteger, .value.i = VA_SURFACE_ATTRIB_USAGE_HINT_DECODER}};
    CHECK(vaCreateSurfaces(display, render_target_format, surface_width, surface_height, &id, 1, &attribute, 1));
    return av_buffer_create((uint8_t *)(uintptr_t)id, sizeof id, free_surface, NULL, 0);
}
static enum AVPixelFormat frames_format; // sw_format of the frames context
static enum AVPixelFormat pick(AVCodecContext *c, const enum AVPixelFormat *f) {
    for (; *f != AV_PIX_FMT_NONE; f++) {
        if (*f != AV_PIX_FMT_VAAPI) continue;
        AVBufferRef *frames = av_hwframe_ctx_alloc(c->hw_device_ctx);
        AVHWFramesContext *fc = (AVHWFramesContext *)frames->data;
        fc->format = AV_PIX_FMT_VAAPI;
        fc->sw_format = frames_format;
        fc->width = surface_width = FFALIGN(c->coded_width, 32);
        fc->height = surface_height = FFALIGN(c->coded_height, 32);
        fc->pool = av_buffer_pool_init2(sizeof(VASurfaceID), NULL, alloc_surface, NULL);
        if (av_hwframe_ctx_init(frames) < 0) { fprintf(stderr, "frames context failed\n"); return AV_PIX_FMT_NONE; }
        c->hw_frames_ctx = frames;
        return *f;
    }
    return AV_PIX_FMT_NONE;
}

int main(int argc, char **argv) {
    if (argc < 6) return 2;
    const char *fourcc_name = argv[5];
    int derive = !strcmp(argv[4], "derive"), limit = argc > 6 ? atoi(argv[6]) : 1 << 30;
    unsigned fourcc = VA_FOURCC(fourcc_name[0], fourcc_name[1], fourcc_name[2], fourcc_name[3]);
    int bytes = (fourcc == VA_FOURCC_P010 || fourcc == VA_FOURCC_P012 || fourcc == VA_FOURCC_Q416) ? 2 : 1;
    int full = fourcc == VA_FOURCC_Q416 || fourcc == VA_FOURCC_444P;
    frames_format = fourcc == VA_FOURCC_NV12 ? AV_PIX_FMT_NV12 : fourcc == VA_FOURCC_P010 ? AV_PIX_FMT_P010 :
                    fourcc == VA_FOURCC_P012 ? AV_PIX_FMT_P012 : AV_PIX_FMT_YUV444P;
    AVFormatContext *fmt = NULL;
    if (avformat_open_input(&fmt, argv[2], NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) return 1;
    int idx = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    // The native decoder: for AV1 the default pick is a software one (libdav1d) that has no hwaccel.
    enum AVCodecID id = fmt->streams[idx]->codecpar->codec_id;
    const AVCodec *codec = id == AV_CODEC_ID_AV1 ? avcodec_find_decoder_by_name("av1") : avcodec_find_decoder(id);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    avcodec_parameters_to_context(dec, fmt->streams[idx]->codecpar);
    int twelve = dec->pix_fmt == AV_PIX_FMT_YUV444P12LE || dec->pix_fmt == AV_PIX_FMT_YUV420P12LE;
    render_target_format = fourcc == VA_FOURCC_NV12 ? VA_RT_FORMAT_YUV420 : fourcc == VA_FOURCC_P010 ? VA_RT_FORMAT_YUV420_10 :
                           fourcc == VA_FOURCC_P012 ? VA_RT_FORMAT_YUV420_12 : fourcc == VA_FOURCC_444P ? VA_RT_FORMAT_YUV444 :
                           twelve ? VA_RT_FORMAT_YUV444_12 : VA_RT_FORMAT_YUV444_10;
    AVBufferRef *dev = NULL;
    if (av_hwdevice_ctx_create(&dev, AV_HWDEVICE_TYPE_VAAPI, argv[1], NULL, 0) < 0) return 1;
    dec->hw_device_ctx = av_buffer_ref(dev);
    dec->get_format = pick;
    if (avcodec_open2(dec, codec, NULL) < 0) return 1;
    VADisplay d = display = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)dev->data)->hwctx)->display;
    FILE *out = fopen(argv[3], "wb");
    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    int frames = 0, finished = 0;
    while (!finished && frames < limit) {
        int more = av_read_frame(fmt, pkt) >= 0;
        if (more && pkt->stream_index != idx) { av_packet_unref(pkt); continue; }
        if (avcodec_send_packet(dec, more ? pkt : NULL) < 0 && more) { fprintf(stderr, "send_packet failed\n"); return 1; }
        av_packet_unref(pkt);
        for (;;) {
            int r = avcodec_receive_frame(dec, frame);
            if (r < 0) { if (!more) finished = 1; break; }
            VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->data[3];
            CHECK(vaSyncSurface(d, surface));
            VAImage image;
            if (derive) {
                CHECK(vaDeriveImage(d, surface, &image));
            } else {
                VAImageFormat format = {.fourcc = fourcc, .byte_order = VA_LSB_FIRST};
                CHECK(vaCreateImage(d, &format, frame->width, frame->height, &image));
                CHECK(vaGetImage(d, surface, 0, 0, frame->width, frame->height, image.image_id));
            }
            if (image.format.fourcc != fourcc) { fprintf(stderr, "image fourcc %.4s, expected %s\n", (char *)&image.format.fourcc, fourcc_name); return 1; }
            unsigned char *base;
            CHECK(vaMapBuffer(d, image.buf, (void **)&base));
            int w = frame->width, h = frame->height;
            for (int plane = 0; plane < image.num_planes; plane++) {
                int rows = plane == 0 || full ? h : h / 2;
                int line = (plane == 0 || full ? w : w) * bytes; // CbCr rows of 4:2:0 hold w samples in total
                for (int row = 0; row < rows; row++)
                    fwrite(base + image.offsets[plane] + (size_t)row * image.pitches[plane], 1, line, out);
            }
            CHECK(vaUnmapBuffer(d, image.buf));
            CHECK(vaDestroyImage(d, image.image_id));
            av_frame_unref(frame);
            frames++;
        }
    }
    fclose(out);
    fprintf(stderr, "%d frames\n", frames);
    return 0;
}
