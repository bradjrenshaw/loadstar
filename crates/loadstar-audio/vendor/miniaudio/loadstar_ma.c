/* miniaudio, compiled once into loadstar-audio, plus a four-function shim so the Rust side
 * never depends on miniaudio's struct layouts. Only the low-level device API is used: miniaudio
 * asks for interleaved f32 stereo frames and Loadstar's own mixer fills them. miniaudio's
 * engine, spatializer, resampler and decoders are compiled out. */
#define MINIAUDIO_IMPLEMENTATION
#define MA_NO_DECODING
#define MA_NO_ENCODING
#define MA_NO_GENERATION
#define MA_NO_RESOURCE_MANAGER
#define MA_NO_NODE_GRAPH
#define MA_NO_ENGINE
#include "miniaudio.h"

typedef void (*ls_ma_callback)(void* user, float* out, ma_uint32 frames);

typedef struct {
    ma_context context;
    ma_device device;
    ls_ma_callback cb;
    void* user;
} ls_ma_handle;

static void ls_ma_data(ma_device* d, void* out, const void* in, ma_uint32 frames)
{
    ls_ma_handle* h = (ls_ma_handle*)d->pUserData;
    (void)in;
    h->cb(h->user, (float*)out, frames);
}

/* Opens the default playback device as f32 stereo. `sample_rate` 0 = the device's native rate;
 * `period_frames` 0 = miniaudio's low-latency default. Returns NULL and sets *out_result on failure. */
ls_ma_handle* ls_ma_open(ma_uint32 sample_rate, ma_uint32 period_frames, ls_ma_callback cb, void* user,
                         ma_uint32* out_rate, ma_uint32* out_period, const char** out_backend, ma_int32* out_result)
{
    ls_ma_handle* h = (ls_ma_handle*)ma_malloc(sizeof(*h), NULL);
    if (h == NULL) { *out_result = MA_OUT_OF_MEMORY; return NULL; }
    MA_ZERO_OBJECT(h);
    h->cb = cb;
    h->user = user;

    ma_context_config ctxcfg = ma_context_config_init();
    ma_result r = ma_context_init(NULL, 0, &ctxcfg, &h->context);
    if (r != MA_SUCCESS) { *out_result = r; ma_free(h, NULL); return NULL; }

    ma_device_config cfg = ma_device_config_init(ma_device_type_playback);
    cfg.playback.format = ma_format_f32;
    cfg.playback.channels = 2;
    cfg.sampleRate = sample_rate;
    cfg.periodSizeInFrames = period_frames;
    cfg.performanceProfile = ma_performance_profile_low_latency;
    cfg.noPreSilencedOutputBuffer = MA_TRUE; /* the mixer always writes every frame */
    cfg.dataCallback = ls_ma_data;
    cfg.pUserData = h;

    r = ma_device_init(&h->context, &cfg, &h->device);
    if (r != MA_SUCCESS) { *out_result = r; ma_context_uninit(&h->context); ma_free(h, NULL); return NULL; }

    *out_rate = h->device.sampleRate;
    *out_period = h->device.playback.internalPeriodSizeInFrames;
    *out_backend = ma_get_backend_name(h->context.backend);
    *out_result = MA_SUCCESS;
    return h;
}

ma_int32 ls_ma_start(ls_ma_handle* h) { return h ? (ma_int32)ma_device_start(&h->device) : MA_INVALID_ARGS; }

void ls_ma_close(ls_ma_handle* h)
{
    if (h == NULL) return;
    ma_device_uninit(&h->device);
    ma_context_uninit(&h->context);
    ma_free(h, NULL);
}

const char* ls_ma_result_description(ma_int32 r) { return ma_result_description((ma_result)r); }
