/* Correlation only: callers serialize access with the device sync lock. */
#ifndef LEGION_EXPORT_STATE_H
#define LEGION_EXPORT_STATE_H
#include <linux/types.h>
#ifndef __KERNEL__
#include <stdbool.h>
#endif

struct nvgpu_export_state {
  __u64 cookie, token, early_token;
  int early_status;
  bool replied;
};

enum nvgpu_export_delivery {
  NVGPU_EXPORT_IGNORE,
  NVGPU_EXPORT_RETAIN,
  NVGPU_EXPORT_COMPLETE,
};

static inline __u64 nvgpu_export_next_value(__u64 previous_cookie) {
  return previous_cookie >= ((1ULL << 63) - 1) ? 0 : previous_cookie + 1;
}

static inline enum nvgpu_export_delivery nvgpu_export_event(
    struct nvgpu_export_state *state, __u32 owner, __u32 event_owner,
    __u64 token, __u64 cookie, int status) {
  if (owner != event_owner || !token || token >= (1ULL << 63) ||
      status == 0 || status > 1 || status < -4095 || cookie != state->cookie)
    return NVGPU_EXPORT_IGNORE;
  if (cookie && !state->replied) {
    if (state->early_token &&
        (state->early_token != token || state->early_status != status))
      return NVGPU_EXPORT_IGNORE;
    state->early_token = token;
    state->early_status = status;
    return NVGPU_EXPORT_RETAIN;
  }
  return token == state->token ? NVGPU_EXPORT_COMPLETE : NVGPU_EXPORT_IGNORE;
}

/* An early event cannot signal success until its token matches the reply. */
static inline bool nvgpu_export_reply(struct nvgpu_export_state *state,
                                      __u64 token, int *ready_status) {
  if (!token || token >= (1ULL << 63) || state->replied ||
      (state->early_token && state->early_token != token))
    return false;
  state->token = token;
  state->replied = true;
  *ready_status = state->early_status;
  return true;
}

static inline bool nvgpu_export_reply_status(struct nvgpu_export_state *state,
                                             __u64 token, int reported_status,
                                             int *ready_status) {
  if (reported_status > 1 || reported_status < -4095 ||
      (reported_status && state->early_status && reported_status != state->early_status))
    return false;
  if (!nvgpu_export_reply(state, token, ready_status))
    return false;
  if (reported_status)
    *ready_status = reported_status;
  return true;
}
#endif
