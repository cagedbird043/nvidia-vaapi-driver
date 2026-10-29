// Surface picture-location transitions: whichever of decode, vaPutImage, VideoProc output or an
// exported write happened last is the picture every reader (vaGetImage, VideoProc, encode) sees.
//
//   surface_state DEVICE
//
// Pictures are flat colours: a decoded H.264 frame (libx264 in process, decoded through the VA
// hwaccel onto a pool surface), a vaPutImage upload, a VideoProc copy of an uploaded surface, or a
// write through the EGL-rendered export. Each case prints "ok NAME" or "FAIL NAME ..." and the
// program exits 1 if any case failed.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <fcntl.h>
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>

#define W 256
#define H 128

struct yuv {
    int y, u, v;
};
static const struct yuv DECODED = {60, 200, 90}, PUT = {180, 80, 160}, PROCESSED = {120, 40, 220},
                        WRITTEN = {100, 150, 50};

static VADisplay dpy;
static AVBufferRef *device;
static AVPacket *stream;
static int failed;
static char fail_reason[256];

#define CHECK(call)                                                                                \
    do {                                                                                           \
        VAStatus s_ = (call);                                                                      \
        if (s_ != VA_STATUS_SUCCESS) {                                                             \
            snprintf(fail_reason, sizeof fail_reason, "%s: %s", #call, vaErrorStr(s_));            \
            return -1;                                                                             \
        }                                                                                          \
    } while (0)
#define TRY(expr)                                                                                  \
    do {                                                                                           \
        if ((expr) < 0)                                                                            \
            return -1;                                                                             \
    } while (0)

static void die(const char *what)
{
    fprintf(stderr, "%s\n", what);
    exit(2);
}

// One intra H.264 frame of the DECODED colour.
static AVPacket *encode_stream(void)
{
    const AVCodec *codec = avcodec_find_encoder_by_name("libx264");
    if (!codec)
        die("libx264 missing");
    AVCodecContext *c = avcodec_alloc_context3(codec);
    c->width = W;
    c->height = H;
    c->pix_fmt = AV_PIX_FMT_YUV420P;
    c->time_base = (AVRational){1, 30};
    c->gop_size = 1;
    av_opt_set(c->priv_data, "preset", "ultrafast", 0);
    av_opt_set(c->priv_data, "qp", "10", 0); // qp 0 would be lossless High 4:4:4
    if (avcodec_open2(c, codec, NULL) < 0)
        die("libx264 open");
    AVFrame *f = av_frame_alloc();
    f->format = c->pix_fmt;
    f->width = W;
    f->height = H;
    av_frame_get_buffer(f, 0);
    memset(f->data[0], DECODED.y, (size_t)f->linesize[0] * H);
    memset(f->data[1], DECODED.u, (size_t)f->linesize[1] * H / 2);
    memset(f->data[2], DECODED.v, (size_t)f->linesize[2] * H / 2);
    AVPacket *p = av_packet_alloc();
    if (avcodec_send_frame(c, f) < 0 || avcodec_send_frame(c, NULL) < 0 || avcodec_receive_packet(c, p) < 0)
        die("libx264 encode");
    av_frame_free(&f);
    avcodec_free_context(&c);
    return p;
}

static enum AVPixelFormat pick_vaapi(AVCodecContext *c, const enum AVPixelFormat *formats)
{
    (void)c;
    for (; *formats != AV_PIX_FMT_NONE; formats++)
        if (*formats == AV_PIX_FMT_VAAPI)
            return *formats;
    return AV_PIX_FMT_NONE;
}

// Decodes the stream onto a pool surface; the decoder and frame keep it alive.
struct decoded {
    AVCodecContext *c;
    AVFrame *f;
    VASurfaceID s;
};
static int decode(struct decoded *d)
{
    d->c = avcodec_alloc_context3(avcodec_find_decoder(AV_CODEC_ID_H264));
    d->c->hw_device_ctx = av_buffer_ref(device);
    d->c->get_format = pick_vaapi;
    d->f = av_frame_alloc();
    if (avcodec_open2(d->c, NULL, NULL) < 0 || avcodec_send_packet(d->c, stream) < 0 || avcodec_send_packet(d->c, NULL) < 0
        || avcodec_receive_frame(d->c, d->f) < 0 || d->f->format != AV_PIX_FMT_VAAPI) {
        snprintf(fail_reason, sizeof fail_reason, "hardware decode");
        return -1;
    }
    d->s = (VASurfaceID)(uintptr_t)d->f->data[3];
    return 0;
}
static void release(struct decoded *d)
{
    av_frame_free(&d->f);
    avcodec_free_context(&d->c);
}
// Decodes the stream again; the pool has to hand back the same surface for the case to mean anything.
static int redecode(struct decoded *d)
{
    av_frame_unref(d->f);
    avcodec_flush_buffers(d->c);
    if (avcodec_send_packet(d->c, stream) < 0 || avcodec_send_packet(d->c, NULL) < 0 || avcodec_receive_frame(d->c, d->f) < 0) {
        snprintf(fail_reason, sizeof fail_reason, "second decode");
        return -1;
    }
    if ((VASurfaceID)(uintptr_t)d->f->data[3] != d->s) {
        snprintf(fail_reason, sizeof fail_reason, "second decode landed on another surface");
        return -1;
    }
    return 0;
}

static int fresh(VASurfaceID *s)
{
    CHECK(vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, s, 1, NULL, 0));
    return 0;
}

static int put(VASurfaceID s, struct yuv c)
{
    VAImageFormat format = {.fourcc = VA_FOURCC_NV12, .byte_order = VA_LSB_FIRST, .bits_per_pixel = 12};
    VAImage image;
    CHECK(vaCreateImage(dpy, &format, W, H, &image));
    unsigned char *p;
    CHECK(vaMapBuffer(dpy, image.buf, (void **)&p));
    for (int y = 0; y < H; y++)
        memset(p + image.offsets[0] + (size_t)y * image.pitches[0], c.y, W);
    for (int y = 0; y < H / 2; y++)
        for (int x = 0; x < W / 2; x++) {
            p[image.offsets[1] + (size_t)y * image.pitches[1] + 2 * x] = c.u;
            p[image.offsets[1] + (size_t)y * image.pitches[1] + 2 * x + 1] = c.v;
        }
    CHECK(vaUnmapBuffer(dpy, image.buf));
    VAStatus status = vaPutImage(dpy, s, image.image_id, 0, 0, W, H, 0, 0, W, H);
    vaDestroyImage(dpy, image.image_id);
    CHECK(status);
    return 0;
}

// VideoProc copy of `src` into `dst`.
static int process(VASurfaceID src, VASurfaceID dst)
{
    VAConfigID config;
    VAContextID context;
    VABufferID buffer;
    CHECK(vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &config));
    CHECK(vaCreateContext(dpy, config, W, H, VA_PROGRESSIVE, &dst, 1, &context));
    VAProcPipelineParameterBuffer pipe = {.surface = src, .output_background_color = 0xff000000};
    CHECK(vaCreateBuffer(dpy, context, VAProcPipelineParameterBufferType, sizeof pipe, 1, &pipe, &buffer));
    CHECK(vaBeginPicture(dpy, context, dst));
    CHECK(vaRenderPicture(dpy, context, &buffer, 1));
    CHECK(vaEndPicture(dpy, context));
    CHECK(vaSyncSurface(dpy, dst));
    vaDestroyContext(dpy, context);
    vaDestroyConfig(dpy, config);
    return 0;
}

static int processed_colour(VASurfaceID dst)
{
    VASurfaceID src;
    TRY(fresh(&src));
    TRY(put(src, PROCESSED));
    int r = process(src, dst);
    vaDestroySurfaces(dpy, &src, 1);
    return r;
}

static int means(VAImage *image, double m[3])
{
    unsigned char *p;
    CHECK(vaMapBuffer(dpy, image->buf, (void **)&p));
    double sum[3] = {0};
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++)
            sum[0] += p[image->offsets[0] + (size_t)y * image->pitches[0] + x];
    for (int y = 0; y < H / 2; y++)
        for (int x = 0; x < W / 2; x++) {
            sum[1] += p[image->offsets[1] + (size_t)y * image->pitches[1] + 2 * x];
            sum[2] += p[image->offsets[1] + (size_t)y * image->pitches[1] + 2 * x + 1];
        }
    CHECK(vaUnmapBuffer(dpy, image->buf));
    m[0] = sum[0] / (W * H);
    m[1] = sum[1] / (W * H / 4);
    m[2] = sum[2] / (W * H / 4);
    return 0;
}

static int compare(const char *how, double m[3], struct yuv want)
{
    if (abs((int)(m[0] + 0.5) - want.y) > 2 || abs((int)(m[1] + 0.5) - want.u) > 2 || abs((int)(m[2] + 0.5) - want.v) > 2) {
        snprintf(fail_reason, sizeof fail_reason, "%s reads %.0f/%.0f/%.0f, want %d/%d/%d", how, m[0], m[1], m[2], want.y,
                 want.u, want.v);
        return -1;
    }
    return 0;
}

// Every reader sees `want`: vaGetImage, vaDeriveImage, and a VideoProc copy (the path encode takes).
static int expect(VASurfaceID s, struct yuv want)
{
    VAImageFormat format = {.fourcc = VA_FOURCC_NV12, .byte_order = VA_LSB_FIRST, .bits_per_pixel = 12};
    VAImage image;
    double m[3];
    CHECK(vaSyncSurface(dpy, s));
    CHECK(vaCreateImage(dpy, &format, W, H, &image));
    VAStatus status = vaGetImage(dpy, s, 0, 0, W, H, image.image_id);
    int r = status == VA_STATUS_SUCCESS ? means(&image, m) : -1;
    vaDestroyImage(dpy, image.image_id);
    CHECK(status);
    TRY(r);
    TRY(compare("vaGetImage", m, want));

    CHECK(vaDeriveImage(dpy, s, &image));
    r = means(&image, m);
    vaDestroyImage(dpy, image.image_id);
    TRY(r);
    TRY(compare("vaDeriveImage", m, want));

    VASurfaceID copy;
    TRY(fresh(&copy));
    r = process(s, copy);
    if (r == 0) {
        CHECK(vaCreateImage(dpy, &format, W, H, &image));
        status = vaGetImage(dpy, copy, 0, 0, W, H, image.image_id);
        r = status == VA_STATUS_SUCCESS ? means(&image, m) : -1;
        vaDestroyImage(dpy, image.image_id);
        CHECK(status);
    }
    vaDestroySurfaces(dpy, &copy, 1);
    TRY(r);
    return compare("VideoProc", m, want);
}

// ---- export and EGL write --------------------------------------------------------------------

static EGLDisplay egl;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_texture;

static int export_surface(VASurfaceID s, uint32_t flags, VADRMPRIMESurfaceDescriptor *desc)
{
    CHECK(vaExportSurfaceHandle(dpy, s, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2, flags | VA_EXPORT_SURFACE_SEPARATE_LAYERS, desc));
    return 0;
}
static void close_export(VADRMPRIMESurfaceDescriptor *desc)
{
    for (unsigned i = 0; i < desc->num_objects; i++)
        close(desc->objects[i].fd);
}

// Clears both exported layers to `c` and waits for GL to finish.
static int write_export(VADRMPRIMESurfaceDescriptor *desc, struct yuv c)
{
    for (unsigned l = 0; l < desc->num_layers; l++) {
        int w = l ? W / 2 : W, h = l ? H / 2 : H;
        int object = desc->layers[l].object_index[0];
        EGLAttrib attrs[] = {EGL_WIDTH, w, EGL_HEIGHT, h, EGL_LINUX_DRM_FOURCC_EXT, desc->layers[l].drm_format,
                             EGL_DMA_BUF_PLANE0_FD_EXT, desc->objects[object].fd,
                             EGL_DMA_BUF_PLANE0_OFFSET_EXT, desc->layers[l].offset[0],
                             EGL_DMA_BUF_PLANE0_PITCH_EXT, desc->layers[l].pitch[0],
                             EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (uint32_t)desc->objects[object].drm_format_modifier,
                             EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (uint32_t)(desc->objects[object].drm_format_modifier >> 32),
                             EGL_NONE};
        EGLImage image = eglCreateImage(egl, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
        if (image == EGL_NO_IMAGE) {
            snprintf(fail_reason, sizeof fail_reason, "eglCreateImage layer %u: %#x", l, eglGetError());
            return -1;
        }
        GLuint texture, framebuffer;
        glGenTextures(1, &texture);
        glBindTexture(GL_TEXTURE_2D, texture);
        image_texture(GL_TEXTURE_2D, image);
        glGenFramebuffers(1, &framebuffer);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
        if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            snprintf(fail_reason, sizeof fail_reason, "layer %u is not renderable", l);
            return -1;
        }
        glViewport(0, 0, w, h);
        if (l)
            glClearColor(c.u / 255.0f, c.v / 255.0f, 0, 1);
        else
            glClearColor(c.y / 255.0f, 0, 0, 1);
        glClear(GL_COLOR_BUFFER_BIT);
        glFinish();
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &texture);
        eglDestroyImage(egl, image);
    }
    return 0;
}

static void egl_init(const char *path)
{
    int fd = open(path, O_RDWR);
    if (fd < 0)
        die("open device");
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    egl = platform(EGL_PLATFORM_GBM_KHR, gbm_create_device(fd), NULL);
    if (!eglInitialize(egl, NULL, NULL))
        die("eglInitialize");
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint attrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext context = eglCreateContext(egl, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attrs);
    if (context == EGL_NO_CONTEXT || !eglMakeCurrent(egl, EGL_NO_SURFACE, EGL_NO_SURFACE, context))
        die("EGL context");
    image_texture = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
}

// ---- cases -----------------------------------------------------------------------------------

static int put_only(void)
{
    VASurfaceID s;
    TRY(fresh(&s));
    int r = put(s, PUT) < 0 ? -1 : expect(s, PUT);
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int decode_then_put(void)
{
    struct decoded d;
    int r = decode(&d);
    if (r == 0)
        r = put(d.s, PUT) < 0 ? -1 : expect(d.s, PUT);
    release(&d);
    return r;
}

static int put_then_decode(void)
{
    struct decoded d;
    TRY(decode(&d));
    // A second decode onto the same pool surface after an upload.
    int r = put(d.s, PUT);
    if (r == 0)
        r = redecode(&d) < 0 ? -1 : expect(d.s, DECODED);
    release(&d);
    return r;
}

static int processed_then_put(void)
{
    VASurfaceID s;
    TRY(fresh(&s));
    int r = processed_colour(s);
    if (r == 0)
        r = put(s, PUT) < 0 ? -1 : expect(s, PUT);
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int decode_then_processed(void)
{
    struct decoded d;
    int r = decode(&d);
    if (r == 0)
        r = processed_colour(d.s) < 0 ? -1 : expect(d.s, PROCESSED);
    release(&d);
    return r;
}

static int put_then_processed(void)
{
    VASurfaceID s;
    TRY(fresh(&s));
    int r = put(s, PUT);
    if (r == 0)
        r = processed_colour(s) < 0 ? -1 : expect(s, PROCESSED);
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int processed_then_decode(void)
{
    struct decoded d;
    TRY(decode(&d));
    int r = processed_colour(d.s);
    if (r == 0)
        r = redecode(&d) < 0 ? -1 : expect(d.s, DECODED);
    release(&d);
    return r;
}

static int processed_then_written(void)
{
    VASurfaceID s;
    VADRMPRIMESurfaceDescriptor desc;
    TRY(fresh(&s));
    int r = processed_colour(s);
    if (r == 0 && (r = export_surface(s, VA_EXPORT_SURFACE_WRITE_ONLY, &desc)) == 0) {
        r = write_export(&desc, WRITTEN) < 0 ? -1 : expect(s, WRITTEN);
        close_export(&desc);
    }
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int put_then_read_write_export(void)
{
    VASurfaceID s;
    VADRMPRIMESurfaceDescriptor desc;
    TRY(fresh(&s));
    int r = put(s, PUT);
    if (r == 0 && (r = export_surface(s, VA_EXPORT_SURFACE_READ_WRITE, &desc)) == 0) {
        // The export shows the upload, and a write through it then replaces it.
        r = expect(s, PUT);
        if (r == 0)
            r = write_export(&desc, WRITTEN) < 0 ? -1 : expect(s, WRITTEN);
        close_export(&desc);
    }
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int decode_then_written(void)
{
    struct decoded d;
    VADRMPRIMESurfaceDescriptor desc;
    int r = decode(&d);
    if (r == 0 && (r = export_surface(d.s, VA_EXPORT_SURFACE_WRITE_ONLY, &desc)) == 0) {
        r = write_export(&desc, WRITTEN) < 0 ? -1 : expect(d.s, WRITTEN);
        close_export(&desc);
    }
    release(&d);
    return r;
}

static int written_then_decode(void)
{
    struct decoded d;
    VADRMPRIMESurfaceDescriptor desc;
    TRY(decode(&d));
    int r = export_surface(d.s, VA_EXPORT_SURFACE_WRITE_ONLY, &desc);
    if (r == 0) {
        r = write_export(&desc, WRITTEN);
        close_export(&desc);
    }
    if (r == 0)
        r = redecode(&d) < 0 ? -1 : expect(d.s, DECODED);
    release(&d);
    return r;
}

static int written_then_processed(void)
{
    VASurfaceID s;
    VADRMPRIMESurfaceDescriptor desc;
    TRY(fresh(&s));
    int r = export_surface(s, VA_EXPORT_SURFACE_WRITE_ONLY, &desc);
    if (r == 0) {
        r = write_export(&desc, WRITTEN);
        close_export(&desc);
    }
    if (r == 0)
        r = processed_colour(s) < 0 ? -1 : expect(s, PROCESSED);
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

static int written_rejects_put(void)
{
    VASurfaceID s;
    VADRMPRIMESurfaceDescriptor desc;
    TRY(fresh(&s));
    int r = export_surface(s, VA_EXPORT_SURFACE_WRITE_ONLY, &desc);
    if (r == 0) {
        r = write_export(&desc, WRITTEN);
        close_export(&desc);
    }
    if (r == 0 && put(s, PUT) == 0) {
        snprintf(fail_reason, sizeof fail_reason, "vaPutImage onto a written export succeeded");
        r = -1;
    }
    if (r == 0)
        r = expect(s, WRITTEN);
    vaDestroySurfaces(dpy, &s, 1);
    return r;
}

int main(int argc, char **argv)
{
    if (argc != 2)
        die("usage: surface_state DEVICE");
    av_log_set_level(AV_LOG_ERROR);
    if (av_hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_VAAPI, argv[1], NULL, 0) < 0)
        die("VA-API device");
    dpy = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)device->data)->hwctx)->display;
    egl_init(argv[1]);
    stream = encode_stream();
    struct {
        const char *name;
        int (*run)(void);
    } cases[] = {
        {"put", put_only},
        {"decode-then-put", decode_then_put},
        {"put-then-decode", put_then_decode},
        {"processed-then-put", processed_then_put},
        {"decode-then-processed", decode_then_processed},
        {"put-then-processed", put_then_processed},
        {"processed-then-decode", processed_then_decode},
        {"processed-then-written", processed_then_written},
        {"put-then-read-write-export-then-written", put_then_read_write_export},
        {"decode-then-written", decode_then_written},
        {"written-then-decode", written_then_decode},
        {"written-then-processed", written_then_processed},
        {"written-rejects-put", written_rejects_put},
    };
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++) {
        fail_reason[0] = 0;
        if (cases[i].run() == 0) {
            printf("ok %s\n", cases[i].name);
        } else {
            printf("FAIL %s %s\n", cases[i].name, fail_reason);
            failed = 1;
        }
    }
    av_packet_free(&stream);
    av_buffer_unref(&device);
    return failed;
}
