// Encodes through FFmpeg's VA-API encoders the way Sunshine does: one encoder input surface is exported
// once with VA_EXPORT_SURFACE_WRITE_ONLY | SEPARATE_LAYERS, its layers are imported into EGL as render
// targets, and every frame GL draws a new flat colour into them (glFlush only) before
// the same surface is sent to the encoder again. The output is decoded in software and each frame's
// mean Y, Cb and Cr must match the colour drawn for it.
// usage: [DMABUF_WRITE_SPIN=n] [DMABUF_WRITE_CYCLES=n] dmabuf_write <render node> <h264_vaapi|hevc_vaapi|av1_vaapi> <NV12|P010> [frames] [out file]
#define EGL_EGLEXT_PROTOTYPES
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <fcntl.h>
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define W 1280
#define H 720
#define FAIL(...) do { fprintf(stderr, __VA_ARGS__); fputc('\n', stderr); exit(1); } while (0)

static int ten;
static void colour(int i, int yuv[3]) {
    // 8-bit studio-range values; distinct for consecutive frames so a stale picture shows.
    yuv[0] = 16 + (i * 37) % 200;
    yuv[1] = 40 + (i * 53) % 170;
    yuv[2] = 200 - (i * 29) % 160;
}

static const char *vertex =
    "#version 300 es\n"
    "void main() { vec2 p = vec2(gl_VertexID == 1 ? 3.0 : -1.0, gl_VertexID == 2 ? 3.0 : -1.0); gl_Position = vec4(p, 0.0, 1.0); }\n";
// The loop only delays completion: the draw finishes long after glFlush returns.
static const char *fragment =
    "#version 300 es\n"
    "precision highp float;\n"
    "uniform vec4 value; uniform int spin;\n"
    "out vec4 colour;\n"
    "void main() { float a = gl_FragCoord.x; for (int i = 0; i < spin; i++) a = fract(a * 1.0001 + 0.37);\n"
    "  colour = value + vec4(a * 1e-30); }\n";

static GLuint shader(GLenum kind, const char *source) {
    GLuint s = glCreateShader(kind);
    glShaderSource(s, 1, &source, NULL);
    glCompileShader(s);
    GLint ok; glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) { char log[1024]; glGetShaderInfoLog(s, sizeof log, NULL, log); FAIL("shader: %s", log); }
    return s;
}

static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_texture;
static GLuint import_layer(EGLDisplay egl, const VADRMPRIMESurfaceDescriptor *d, int layer, int w, int h) {
    int object = d->layers[layer].object_index[0];
    EGLAttrib attributes[] = {
        EGL_WIDTH, w, EGL_HEIGHT, h, EGL_LINUX_DRM_FOURCC_EXT, d->layers[layer].drm_format,
        EGL_DMA_BUF_PLANE0_FD_EXT, d->objects[object].fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, d->layers[layer].offset[0],
        EGL_DMA_BUF_PLANE0_PITCH_EXT, d->layers[layer].pitch[0],
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (uint32_t)d->objects[object].drm_format_modifier,
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (uint32_t)(d->objects[object].drm_format_modifier >> 32),
        EGL_NONE};
    EGLImage image = eglCreateImage(egl, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (!image) FAIL("eglCreateImage layer %d fourcc %.4s: %#x", layer, (char *)&d->layers[layer].drm_format, eglGetError());
    GLuint texture, framebuffer;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    target_texture(GL_TEXTURE_2D, (GLeglImageOES)image);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) FAIL("layer %d is not renderable", layer);
    return framebuffer;
}

// Mean of a plane of the decoded frame, in 8-bit units.
static double mean(const AVFrame *f, int plane) {
    int w = plane ? f->width / 2 : f->width, h = plane ? f->height / 2 : f->height;
    double sum = 0;
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++)
            sum += ten ? ((const uint16_t *)(f->data[plane] + (size_t)y * f->linesize[plane]))[x] / 4.0
                       : f->data[plane][(size_t)y * f->linesize[plane] + x];
    return sum / ((double)w * h);
}

static long rss_kb(void) { FILE *f = fopen("/proc/self/statm", "r"); long a, b; if (fscanf(f, "%ld %ld", &a, &b) != 2) b = 0; fclose(f); return b * 4; }
static int fd_count(void) { char cmd[64]; int n = 0; snprintf(cmd, sizeof cmd, "ls /proc/%d/fd | wc -l", getpid()); FILE *f = popen(cmd, "r"); if (fscanf(f, "%d", &n) != 1) n = 0; pclose(f); return n; }
static long gpu_used_mib(void) { long n = -1; FILE *f = popen("nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits -i 0", "r"); if (!f || fscanf(f, "%ld", &n) != 1) n = -1; if (f) pclose(f); return n; }
// Writable export / destroy cycles must not grow the process's memory, descriptors or GPU memory.
static int cycles(VADisplay va, int count, unsigned rt) {
    long rss = 0, gpu = 0; int fds = 0;
    for (int i = 0; i < count; i++) {
        if (i == 20) { rss = rss_kb(); fds = fd_count(); gpu = gpu_used_mib(); }
        VASurfaceID s;
        if (vaCreateSurfaces(va, rt, W, H, &s, 1, NULL, 0)) FAIL("vaCreateSurfaces");
        VADRMPRIMESurfaceDescriptor desc;
        VAStatus status = vaExportSurfaceHandle(va, s, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                                                VA_EXPORT_SURFACE_READ_WRITE | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &desc);
        if (status) FAIL("cycle export: %s", vaErrorStr(status));
        for (uint32_t o = 0; o < desc.num_objects; o++) close(desc.objects[o].fd);
        vaDestroySurfaces(va, &s, 1);
    }
    long rss2 = rss_kb(), gpu2 = gpu_used_mib(); int fds2 = fd_count();
    printf("cycles %d: rss %ld -> %ld KiB, fds %d -> %d, GPU %ld -> %ld MiB\n", count, rss, rss2, fds, fds2, gpu, gpu2);
    return fds2 > fds || rss2 > rss + 4096 || gpu2 > gpu + 16;
}
int main(int argc, char **argv) {
    if (argc < 4) return 2;
    const char *encoder_name = argv[2];
    ten = !strcmp(argv[3], "P010");
    int limit = argc > 4 ? atoi(argv[4]) : 60;
    FILE *dump = argc > 5 ? fopen(argv[5], "wb") : NULL;
    // DMABUF_WRITE_SPIN: per-pixel loop iterations that keep each draw on the GPU after glFlush. NVIDIA's
    // GL fences nothing on the dma-buf, so a draw still running when the encoder ingests is not waited for.
    int spin_count = getenv("DMABUF_WRITE_SPIN") ? atoi(getenv("DMABUF_WRITE_SPIN")) : 0;

    int fd = open(argv[1], O_RDWR);
    if (fd < 0) FAIL("open %s", argv[1]);
    struct gbm_device *gbm = gbm_create_device(fd);
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay egl = get_display(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    if (!eglInitialize(egl, &major, &minor)) FAIL("eglInitialize failed");
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext context = eglCreateContext(egl, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, context_attributes);
    if (!context || !eglMakeCurrent(egl, EGL_NO_SURFACE, EGL_NO_SURFACE, context)) FAIL("EGL context: %#x", eglGetError());
    target_texture = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    fprintf(stderr, "EGL %d.%d renderer %s\n", major, minor, glGetString(GL_RENDERER));

    AVBufferRef *device = NULL;
    if (av_hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_VAAPI, argv[1], NULL, 0) < 0) FAIL("VA-API device");
    VADisplay va = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)device->data)->hwctx)->display;
    fprintf(stderr, "vendor %s\n", vaQueryVendorString(va));
    // DMABUF_WRITE_CYCLES: writable export / destroy cycles run first (RSS, descriptors, GPU memory).
    int leaked = getenv("DMABUF_WRITE_CYCLES") && cycles(va, atoi(getenv("DMABUF_WRITE_CYCLES")), ten ? VA_RT_FORMAT_YUV420_10 : VA_RT_FORMAT_YUV420);
    AVBufferRef *frames = av_hwframe_ctx_alloc(device);
    AVHWFramesContext *fc = (AVHWFramesContext *)frames->data;
    fc->format = AV_PIX_FMT_VAAPI;
    fc->sw_format = ten ? AV_PIX_FMT_P010 : AV_PIX_FMT_NV12;
    fc->width = W;
    fc->height = H;
    fc->initial_pool_size = 4;
    if (av_hwframe_ctx_init(frames) < 0) FAIL("hwframe context");

    const AVCodec *codec = avcodec_find_encoder_by_name(encoder_name);
    if (!codec) FAIL("no encoder %s", encoder_name);
    AVCodecContext *enc = avcodec_alloc_context3(codec);
    enc->width = W;
    enc->height = H;
    enc->time_base = (AVRational){1, 60};
    enc->framerate = (AVRational){60, 1};
    enc->pix_fmt = AV_PIX_FMT_VAAPI;
    enc->hw_frames_ctx = av_buffer_ref(frames);
    enc->max_b_frames = 0;
    enc->gop_size = 30;
    enc->global_quality = 18;
    if (ten && !strcmp(encoder_name, "hevc_vaapi")) enc->profile = AV_PROFILE_HEVC_MAIN_10;
    enc->color_range = AVCOL_RANGE_MPEG;
    av_opt_set_int(enc->priv_data, "async_depth", 1, 0);
    av_opt_set(enc->priv_data, "rc_mode", "CQP", 0);
    if (avcodec_open2(enc, codec, NULL) < 0) FAIL("open %s", encoder_name);

    AVFrame *frame = av_frame_alloc();
    if (av_hwframe_get_buffer(frames, frame, 0) < 0) FAIL("hwframe");
    VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->data[3];
    VADRMPRIMESurfaceDescriptor desc;
    VAStatus status = vaExportSurfaceHandle(va, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                                            VA_EXPORT_SURFACE_WRITE_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &desc);
    if (status) FAIL("vaExportSurfaceHandle: %s", vaErrorStr(status));
    if (desc.num_layers != 2) FAIL("expected 2 layers, got %u", desc.num_layers);
    fprintf(stderr, "export %.4s %ux%u layers %.4s %.4s modifier %#llx\n", (char *)&desc.fourcc, desc.width, desc.height,
            (char *)&desc.layers[0].drm_format, (char *)&desc.layers[1].drm_format,
            (unsigned long long)desc.objects[0].drm_format_modifier);
    GLuint targets[2] = {import_layer(egl, &desc, 0, desc.width, desc.height),
                         import_layer(egl, &desc, 1, desc.width / 2, desc.height / 2)};
    for (uint32_t i = 0; i < desc.num_objects; i++) close(desc.objects[i].fd);

    GLuint program = glCreateProgram();
    glAttachShader(program, shader(GL_VERTEX_SHADER, vertex));
    glAttachShader(program, shader(GL_FRAGMENT_SHADER, fragment));
    glLinkProgram(program);
    glUseProgram(program);
    GLint value = glGetUniformLocation(program, "value"), spin = glGetUniformLocation(program, "spin");
    GLuint vao; glGenVertexArrays(1, &vao); glBindVertexArray(vao);

    const AVCodec *decoder_codec = avcodec_find_decoder(enc->codec_id == AV_CODEC_ID_AV1 ? AV_CODEC_ID_AV1 : enc->codec_id);
    if (enc->codec_id == AV_CODEC_ID_AV1) decoder_codec = avcodec_find_decoder_by_name("libdav1d") ?: decoder_codec;
    AVCodecContext *dec = avcodec_alloc_context3(decoder_codec);
    if (avcodec_open2(dec, decoder_codec, NULL) < 0) FAIL("open decoder");

    AVPacket *pkt = av_packet_alloc();
    AVFrame *out = av_frame_alloc();
    int decoded = 0, bad = 0, sent = 0;
    double scale = ten ? 64.0 * 4.0 / 65535.0 : 1.0 / 255.0; // 8-bit value -> normalized, 10 bits in the high bits of 16
    for (int i = 0; i <= limit; i++) {
        if (i < limit) {
            int yuv[3];
            colour(i, yuv);
            glUniform1i(spin, spin_count);
            glBindFramebuffer(GL_FRAMEBUFFER, targets[0]);
            glViewport(0, 0, desc.width, desc.height);
            glUniform4f(value, yuv[0] * scale, 0, 0, 1);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glBindFramebuffer(GL_FRAMEBUFFER, targets[1]);
            glViewport(0, 0, desc.width / 2, desc.height / 2);
            glUniform4f(value, yuv[1] * scale, yuv[2] * scale, 0, 1);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glFlush();
            frame->pts = i;
            if (avcodec_send_frame(enc, frame) < 0) FAIL("send frame %d", i);
            sent++;
        } else {
            avcodec_send_frame(enc, NULL);
        }
        while (avcodec_receive_packet(enc, pkt) == 0) {
            if (dump) fwrite(pkt->data, 1, pkt->size, dump);
            if (avcodec_send_packet(dec, pkt) < 0) FAIL("decode packet");
            av_packet_unref(pkt);
            while (avcodec_receive_frame(dec, out) == 0) {
                int want[3];
                colour(decoded, want);
                double got[3] = {mean(out, 0), mean(out, 1), mean(out, 2)};
                int ok = 1;
                for (int c = 0; c < 3; c++) ok &= got[c] > want[c] - 2.5 && got[c] < want[c] + 2.5;
                if (!ok) {
                    if (bad < 8)
                        fprintf(stderr, "frame %d: drew %d/%d/%d, decoded %.1f/%.1f/%.1f\n", decoded, want[0], want[1], want[2], got[0], got[1], got[2]);
                    bad++;
                }
                decoded++;
                av_frame_unref(out);
            }
        }
    }
    avcodec_send_packet(dec, NULL);
    while (avcodec_receive_frame(dec, out) == 0) {
        int want[3];
        colour(decoded, want);
        double got[3] = {mean(out, 0), mean(out, 1), mean(out, 2)};
        int ok = 1;
        for (int c = 0; c < 3; c++) ok &= got[c] > want[c] - 2.5 && got[c] < want[c] + 2.5;
        bad += !ok;
        decoded++;
        av_frame_unref(out);
    }
    if (dump) fclose(dump);
    printf("%s %s: %d sent, %d decoded, %d wrong\n", encoder_name, argv[3], sent, decoded, bad);
    return bad || decoded != sent || leaked;
}
