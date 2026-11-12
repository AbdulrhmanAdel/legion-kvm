/* Guest-only adapter. */
#include <linux/dma-fence-array.h>
#include <linux/dma-resv.h>
#include "legion_export_state.h"
#define NVGPU_MAX_WAIT_INPUTS 32
static struct workqueue_struct *nvgpu_sync_wq;
/* Paired backend/module experiment; default preserves event-only exports. */
static int nvgpu_sync_inline_ready;
module_param_named(sync_inline_ready, nvgpu_sync_inline_ready, int, 0444);
MODULE_PARM_DESC(sync_inline_ready, "return verified terminal fence status with export");
static int nvgpu_sync_export_arm;
module_param_named(sync_export_arm, nvgpu_sync_export_arm, int, 0444);
MODULE_PARM_DESC(sync_export_arm, "combine export and arm using a registered guest proxy");
static atomic64_t nvgpu_sync_cookie = ATOMIC64_INIT(0);

struct nvgpu_sync_fence {
  struct dma_fence base;
  spinlock_t lock;
  struct list_head node;
  struct nvgpu_device *dev;
  struct file *owner_file;
  u32 owner;
  struct nvgpu_export_state export_state;
  struct delayed_work watchdog;
  struct work_struct cleanup;
  struct work_struct owner_release;
  bool release_owner_on_signal;
};

static void nvgpu_device_free(struct kref *ref) {
  struct nvgpu_device *dev = container_of(ref, struct nvgpu_device, lifetime);
  put_device(&dev->vdev->dev);
  kfree(dev);
}

static void nvgpu_device_put(struct nvgpu_device *dev) {
  kref_put(&dev->lifetime, nvgpu_device_free);
}

static void nvgpu_device_put_action(void *data) { nvgpu_device_put(data); }

static const char *nvgpu_sync_driver(struct dma_fence *fence) {
  return "legion";
}
static const char *nvgpu_sync_timeline(struct dma_fence *fence) {
  return "nvidia.host";
}
static bool nvgpu_sync_enable(struct dma_fence *fence) { return true; }

/* May sleep. */
static void nvgpu_sync_cleanup(struct work_struct *work) {
  struct nvgpu_sync_fence *nf =
      container_of(work, struct nvgpu_sync_fence, cleanup);
  cancel_delayed_work_sync(&nf->watchdog);
  if (nf->export_state.token) {
    __le64 token = cpu_to_le64(nf->export_state.token);
    nvgpu_ioctl_flat_h(nf->dev, nf->owner, 0x40084c22, &token, sizeof(token));
  }
  if (nf->owner_file)
    fput(nf->owner_file);
  nvgpu_device_put(nf->dev);
  /* Fence readers may still hold an RCU read-side reference after the last counted put. */
  dma_fence_free(&nf->base);
  module_put(THIS_MODULE);
}

static void nvgpu_sync_release(struct dma_fence *fence) {
  struct nvgpu_sync_fence *nf =
      container_of(fence, struct nvgpu_sync_fence, base);
  queue_work(nvgpu_sync_wq, &nf->cleanup);
}

static const struct dma_fence_ops nvgpu_sync_ops = {
    .get_driver_name = nvgpu_sync_driver,
    .get_timeline_name = nvgpu_sync_timeline,
    .enable_signaling = nvgpu_sync_enable,
    .release = nvgpu_sync_release,
#ifdef NVGPU_HAVE_FENCE_OPS64
    .use_64bit_seqno = true,
#endif
};

/* Reservation -> proxy -> render file -> GEM table -> reservation must not become a permanent reference cycle. */
static void nvgpu_sync_release_owner(struct work_struct *work) {
  struct nvgpu_sync_fence *nf = container_of(work, struct nvgpu_sync_fence,
                                             owner_release);
  struct file *owner = nf->owner_file;
  nf->owner_file = NULL;
  fput(owner);
  dma_fence_put(&nf->base);
}

/* Exactly one caller consumes the registry reference. */
static void nvgpu_sync_finish(struct nvgpu_sync_fence *nf, int status) {
  unsigned long flags;
  spin_lock_irqsave(&nf->dev->sync_lock, flags);
  if (list_empty(&nf->node)) {
    spin_unlock_irqrestore(&nf->dev->sync_lock, flags);
    return;
  }
  list_del_init(&nf->node);
  spin_unlock_irqrestore(&nf->dev->sync_lock, flags);
  if (cancel_delayed_work(&nf->watchdog))
    dma_fence_put(&nf->base); /* canceled watchdog's reference */
  if (status < 0)
    dma_fence_set_error(&nf->base, status);
  dma_fence_signal(&nf->base);
  if (nf->release_owner_on_signal) {
    dma_fence_get(&nf->base);
    if (!queue_work(nvgpu_sync_wq, &nf->owner_release))
      dma_fence_put(&nf->base);
  }
  dma_fence_put(&nf->base); /* registry reference */
}

static void nvgpu_sync_watchdog(struct work_struct *work) {
  struct nvgpu_sync_fence *nf =
      container_of(to_delayed_work(work), struct nvgpu_sync_fence, watchdog);
  /* Lost transport is an error, never evidence of successful GPU completion. */
  nvgpu_sync_finish(nf, -ETIMEDOUT);
  dma_fence_put(&nf->base); /* running watchdog's reference */
}

static void nvgpu_sync_event(struct nvgpu_device *dev,
                             struct nvgpu_event_buf *packet, unsigned int len) {
  struct nvgpu_sync_fence *nf, *found = NULL;
  unsigned long flags;
  u64 token, cookie;
  int status;
  if (len != 32)
    return;
  token = get_unaligned_le64(packet->display_payload);
  cookie = get_unaligned_le64(packet->display_payload + 8);
  if (cookie && !nvgpu_sync_export_arm)
    return;
  status = (s32)le32_to_cpu((__le32)packet->hdr.status);
  if (status == 0 || status > 1 || status < -MAX_ERRNO)
    return;
  spin_lock_irqsave(&dev->sync_lock, flags);
  list_for_each_entry(nf, &dev->sync_fences, node) {
    enum nvgpu_export_delivery delivery = nvgpu_export_event(
        &nf->export_state, nf->owner, le32_to_cpu(packet->hdr.handle),
        token, cookie, status);
    if (delivery == NVGPU_EXPORT_RETAIN)
      break;
    if (delivery == NVGPU_EXPORT_COMPLETE) {
      dma_fence_get(&nf->base); /* local reference across unlocked finish */
      found = nf;
      break;
    }
  }
  spin_unlock_irqrestore(&dev->sync_lock, flags);
  if (found) {
    nvgpu_sync_finish(found, status);
    dma_fence_put(&found->base);
  }
}

static void nvgpu_sync_stop(struct nvgpu_device *dev) {
  struct nvgpu_sync_fence *nf;
  unsigned long flags;
  /* Complete existing synchronous calls before resetting their queue. */
  down_write(&dev->transport_lifetime);
  spin_lock_irqsave(&dev->sync_lock, flags);
  dev->stopping = true;
  spin_unlock_irqrestore(&dev->sync_lock, flags);
  up_write(&dev->transport_lifetime);
  for (;;) {
    spin_lock_irqsave(&dev->sync_lock, flags);
    if (list_empty(&dev->sync_fences)) {
      spin_unlock_irqrestore(&dev->sync_lock, flags);
      break;
    }
    nf = list_first_entry(&dev->sync_fences, struct nvgpu_sync_fence, node);
    dma_fence_get(&nf->base);
    spin_unlock_irqrestore(&dev->sync_lock, flags);
    nvgpu_sync_finish(nf, -ENODEV);
    dma_fence_put(&nf->base);
  }
}

static int nvgpu_sync_input_token(struct nvgpu_fd *nfd, struct dma_fence *fence,
                                  u64 *token) {
  struct nvgpu_sync_fence *input;
  if (rcu_access_pointer(fence->ops) != &nvgpu_sync_ops)
    return -EOPNOTSUPP;
  input = container_of(fence, struct nvgpu_sync_fence, base);
  if (input->dev != nfd->dev || input->owner != nfd->handle)
    return -EXDEV;
  *token = input->export_state.token;
  return 0;
}

static long nvgpu_sync_completed_wait(struct nvgpu_fd *nfd,
                                       struct nvgpu_gem_object *context,
                                       u64 pre, u64 post) {
  u8 request[24] = {};
  put_unaligned_le32(context->host_handle, request);
  put_unaligned_le64(pre, request + 8);
  put_unaligned_le64(post, request + 16);
  return nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, 0x40184c2a, request,
                            sizeof(request));
}

/* Pending inputs require owned bridge leaves. */
static long nvgpu_sync_wait(struct nvgpu_fd *nfd, struct drm_file *file,
                            unsigned int cmd, void __user *uarg) {
  struct {
    __u32 context;
    __s32 fd;
    __u64 pre, post;
  } params;
  struct drm_gem_object *object;
  struct nvgpu_gem_object *context;
  struct dma_fence *fence;
  u8 request[32] = {};
  u8 many[32 + NVGPU_MAX_WAIT_INPUTS * 8] = {};
  u64 token;
  long ret;
  if (!nvgpu_fence_probe || cmd != 0x40186456 || !file)
    return -EOPNOTSUPP;
  if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;
  if (params.pre >= params.post)
    return -EINVAL;
  object = drm_gem_object_lookup(file, params.context);
  if (!object)
    return -ENOENT;
  ret = -EINVAL;
  if (object->funcs != &nvgpu_gem_funcs ||
      !to_nvgpu_gem(object)->fence_context)
    goto put_object;
  context = to_nvgpu_gem(object);
  if (context->owner_handle != nfd->handle) {
    ret = -EXDEV;
    goto put_object;
  }
  fence = sync_file_get_fence(params.fd);
  if (!fence)
    goto put_object;
  /* Keep the existing one-ioctl path for directly reusable native fences, including already-completed owned inputs. */
  if (rcu_access_pointer(fence->ops) == &nvgpu_sync_ops) {
    ret = nvgpu_sync_input_token(nfd, fence, &token);
    if (!ret)
      goto single_input;
    if (!dma_fence_is_signaled(fence))
      goto put_input;
  }
  if (dma_fence_is_signaled(fence)) {
    ret = nvgpu_sync_completed_wait(nfd, context, params.pre, params.post);
    goto put_input;
  }
  if (dma_fence_is_array(fence)) {
    struct dma_fence *leaf;
    unsigned int index, count = 0;
    if (to_dma_fence_array(fence)->num_fences > NVGPU_MAX_WAIT_INPUTS) {
      ret = -E2BIG;
      goto put_input;
    }
    dma_fence_array_for_each(leaf, index, fence) {
      if (dma_fence_is_signaled(leaf))
        continue;
      if (count == NVGPU_MAX_WAIT_INPUTS) {
        ret = -E2BIG;
        goto put_input;
      }
      ret = nvgpu_sync_input_token(nfd, leaf, &token);
      if (ret)
        goto put_input;
      put_unaligned_le64(token, many + 32 + count * 8);
      count++;
    }
    if (!count) {
      ret = nvgpu_sync_completed_wait(nfd, context, params.pre, params.post);
      goto put_input;
    }
    put_unaligned_le32(context->host_handle, many);
    put_unaligned_le32(count, many + 4);
    put_unaligned_le64(params.pre, many + 8);
    put_unaligned_le64(params.post, many + 16);
    /* The counted array reference retains every leaf and its host token until the backend has installed a native merged-fence wait. */
    ret = nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, 0x41204c28, many,
                            sizeof(many));
    goto put_input;
  }
  ret = nvgpu_sync_input_token(nfd, fence, &token);
  if (ret)
    goto put_input;
single_input:
  put_unaligned_le32(context->host_handle, request);
  put_unaligned_le64(token, request + 8);
  put_unaligned_le64(params.pre, request + 16);
  put_unaligned_le64(params.post, request + 24);
  /* Both GEM context and input fence remain referenced through the RPC. */
  ret = nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, 0x40204c27, request,
                          sizeof(request));
put_input:
  dma_fence_put(fence);
put_object:
  drm_gem_object_put(object);
  return ret;
}

static int nvgpu_sync_register(struct nvgpu_sync_fence *nf, u32 timeout_ms) {
  unsigned long flags;
  unsigned int timeout = timeout_ms ? min_t(unsigned int, timeout_ms, 5000) : 5000;
  spin_lock_irqsave(&nf->dev->sync_lock, flags);
  if (nf->dev->stopping) {
    spin_unlock_irqrestore(&nf->dev->sync_lock, flags);
    return -ENODEV;
  }
  dma_fence_get(&nf->base); /* registry */
  list_add_tail(&nf->node, &nf->dev->sync_fences);
  dma_fence_get(&nf->base); /* watchdog */
  queue_delayed_work(nvgpu_sync_wq, &nf->watchdog,
                     msecs_to_jiffies(timeout + 1000));
  spin_unlock_irqrestore(&nf->dev->sync_lock, flags);
  return 0;
}

static u64 nvgpu_sync_new_cookie(void) {
  u64 previous_cookie = atomic64_read(&nvgpu_sync_cookie);
  for (;;) {
    u64 next = nvgpu_export_next_value(previous_cookie);
    u64 observed;
    if (!next)
      return 0; /* Exhausted cookies are never reset or reused. */
    observed = atomic64_cmpxchg(&nvgpu_sync_cookie, previous_cookie, next);
    if (observed == previous_cookie)
      return next;
    previous_cookie = observed;
  }
}

/* Caller retains a validated context and receives one local fence reference. */
static struct nvgpu_sync_fence *nvgpu_sync_export(
    struct nvgpu_fd *nfd, struct drm_file *file,
    struct nvgpu_gem_object *context, u32 timeout_ms, u64 wait,
    bool release_owner_on_signal) {
  struct nvgpu_sync_fence *nf;
  u8 request[32] = {};
  unsigned long flags;
  bool registered = false;
  int initial_status = 0;
  long ret;
  nf = kzalloc(sizeof(*nf), GFP_KERNEL);
  if (!nf)
    return ERR_PTR(-ENOMEM);
  nf->dev = nfd->dev;
  kref_get(&nf->dev->lifetime);
  __module_get(THIS_MODULE);
  nf->owner_file = get_file(file->filp);
  nf->owner = nfd->handle;
  spin_lock_init(&nf->lock);
  INIT_LIST_HEAD(&nf->node);
  INIT_WORK(&nf->cleanup, nvgpu_sync_cleanup);
  INIT_WORK(&nf->owner_release, nvgpu_sync_release_owner);
  nf->release_owner_on_signal = release_owner_on_signal;
  INIT_DELAYED_WORK(&nf->watchdog, nvgpu_sync_watchdog);
#ifdef NVGPU_HAVE_FENCE_INIT64
  dma_fence_init64(&nf->base, &nvgpu_sync_ops, &nf->lock, context->sync_context,
                   wait);
#elif defined(NVGPU_HAVE_FENCE_OPS64)
  dma_fence_init(&nf->base, &nvgpu_sync_ops, &nf->lock, context->sync_context,
                 wait);
#else
#error "no qualified 64-bit dma-fence initialization interface"
#endif
  put_unaligned_le32(context->host_handle, request);
  put_unaligned_le32(timeout_ms, request + 4);
  put_unaligned_le64(wait, request + 8);
  if (nvgpu_sync_export_arm) {
    nf->export_state.cookie = nvgpu_sync_new_cookie();
    if (!nf->export_state.cookie) {
      ret = -EOVERFLOW;
      goto put_fence;
    }
    put_unaligned_le64(nf->export_state.cookie, request + 16);
    ret = nvgpu_sync_register(nf, timeout_ms);
    if (ret)
      goto put_fence;
    registered = true;
  }
  ret = nvgpu_ioctl_flat_h(nf->dev, nf->owner,
                           nvgpu_sync_export_arm ?
                             (nvgpu_sync_inline_ready ? 0xc0204c2d : 0xc0204c2c) :
                           nvgpu_sync_inline_ready ? 0xc0204c2b : 0xc0204c20, request,
                           sizeof(request));
  if (ret)
    goto put_fence;
  /* An event can already be retained in export_state. */
  spin_lock_irqsave(&nf->dev->sync_lock, flags);
  if ((nvgpu_sync_export_arm && get_unaligned_le32(request + 28)) ||
      !nvgpu_export_reply_status(&nf->export_state, get_unaligned_le64(request + 16),
          nvgpu_sync_export_arm && nvgpu_sync_inline_ready ?
            (s32)get_unaligned_le32(request + 24) : 0, &initial_status))
    ret = -EPROTO;
  spin_unlock_irqrestore(&nf->dev->sync_lock, flags);
  if (ret)
    goto put_fence;
  if (nvgpu_sync_export_arm) {
    if (initial_status)
      nvgpu_sync_finish(nf, initial_status);
    return nf;
  }
  if (nvgpu_sync_inline_ready) {
    initial_status = (s32)get_unaligned_le32(request + 24);
    if (initial_status > 1 || initial_status < -MAX_ERRNO ||
        get_unaligned_le32(request + 28)) {
      ret = -EPROTO;
      goto put_fence;
    }
  }
  ret = nvgpu_sync_register(nf, timeout_ms);
  if (ret)
    goto put_fence;
  registered = true;
  if (initial_status) {
    /* Native terminal status was observed, not inferred from a semaphore value or elapsed time. */
    nvgpu_sync_finish(nf, initial_status);
    return nf;
  }
  {
    __le64 token = cpu_to_le64(nf->export_state.token);
    ret = nvgpu_ioctl_flat_h(nf->dev, nf->owner, 0x40084c24, &token,
                             sizeof(token));
  }
  if (ret) {
    nvgpu_sync_finish(nf, ret < 0 ? ret : -EIO);
    goto put_fence;
  }
  return nf;
put_fence:
  if (registered)
    nvgpu_sync_finish(nf, ret < 0 ? ret : -EIO);
  dma_fence_put(&nf->base);
  return ERR_PTR(ret);
}

static long nvgpu_sync_create(struct nvgpu_fd *nfd, struct drm_file *file,
                              unsigned int cmd, void __user *uarg) {
  struct { __u32 context, timeout; __u64 wait; __s32 fd; __u32 pad; } params;
  struct drm_gem_object *object;
  struct nvgpu_gem_object *context;
  struct nvgpu_sync_fence *nf;
  struct sync_file *sync;
  int fd;
  long ret;
  if (!nvgpu_fence_probe || cmd != 0xc0186455 || !file)
    return -EOPNOTSUPP;
  if (copy_from_user(&params, uarg, sizeof(params)))
    return -EFAULT;
  if (params.pad)
    return -EINVAL;
  object = drm_gem_object_lookup(file, params.context);
  if (!object)
    return -ENOENT;
  ret = -EINVAL;
  if (object->funcs != &nvgpu_gem_funcs || !to_nvgpu_gem(object)->fence_context)
    goto put_object;
  context = to_nvgpu_gem(object);
  if (context->owner_handle != nfd->handle) { ret = -EXDEV; goto put_object; }
  fd = get_unused_fd_flags(O_CLOEXEC);
  if (fd < 0) { ret = fd; goto put_object; }
  nf = nvgpu_sync_export(nfd, file, context, params.timeout, params.wait, false);
  if (IS_ERR(nf)) { ret = PTR_ERR(nf); goto put_fd; }
  sync = sync_file_create(&nf->base);
  if (!sync) {
    ret = -ENOMEM;
    nvgpu_sync_finish(nf, ret);
    goto put_fence;
  }
  params.fd = fd;
  if (copy_to_user(uarg, &params, sizeof(params))) {
    nvgpu_sync_finish(nf, -ECANCELED);
    ret = -EFAULT;
    goto put_sync;
  }
  fd_install(fd, sync->file);
  dma_fence_put(&nf->base); /* local */
  drm_gem_object_put(object);
  return 0;
put_sync:
  fput(sync->file);
put_fence:
  dma_fence_put(&nf->base);
put_fd:
  put_unused_fd(fd);
put_object:
  drm_gem_object_put(object);
  return ret;
}

static long nvgpu_sync_attach(struct nvgpu_fd *nfd, struct drm_file *file,
                              unsigned int cmd, void __user *uarg) {
  struct { __u32 buffer, context, timeout, shared; __u64 wait; } params;
  struct drm_gem_object *buffer, *object;
  struct nvgpu_gem_object *context;
  struct nvgpu_sync_fence *nf;
  u8 request[24] = {};
  long ret;
  if (!nvgpu_fence_probe || cmd != 0x40186457 || !file)
    return -EOPNOTSUPP;
  if (copy_from_user(&params, uarg, sizeof(params))) return -EFAULT;
  if (params.shared > 1) return -EINVAL;
  buffer = drm_gem_object_lookup(file, params.buffer);
  if (!buffer) return -ENOENT;
  object = drm_gem_object_lookup(file, params.context);
  if (!object) { ret = -ENOENT; goto put_buffer; }
  ret = -EINVAL;
  if (buffer->funcs != &nvgpu_gem_funcs || to_nvgpu_gem(buffer)->fence_context ||
      object->funcs != &nvgpu_gem_funcs || !to_nvgpu_gem(object)->fence_context)
    goto put_context;
  context = to_nvgpu_gem(object);
  if (to_nvgpu_gem(buffer)->owner_handle != nfd->handle ||
      context->owner_handle != nfd->handle) { ret = -EXDEV; goto put_context; }
  ret = dma_resv_lock(buffer->resv, NULL);
  if (ret) goto put_context;
  ret = dma_resv_reserve_fences(buffer->resv, 1);
  if (ret) goto unlock;
  nf = nvgpu_sync_export(nfd, file, context, params.timeout, params.wait, true);
  if (IS_ERR(nf)) { ret = PTR_ERR(nf); goto unlock; }
  put_unaligned_le32(to_nvgpu_gem(buffer)->host_handle, request);
  put_unaligned_le32(params.shared, request + 4);
  put_unaligned_le64(nf->export_state.token, request + 8);
  ret = nvgpu_ioctl_flat_h(nfd->dev, nfd->handle, 0x40184c29, request, sizeof(request));
  if (!ret)
    dma_resv_add_fence(buffer->resv, &nf->base,
                       params.shared ? DMA_RESV_USAGE_READ : DMA_RESV_USAGE_WRITE);
  else
    nvgpu_sync_finish(nf, ret < 0 ? ret : -EIO);
  dma_fence_put(&nf->base);
unlock:
  dma_resv_unlock(buffer->resv);
put_context:
  drm_gem_object_put(object);
put_buffer:
  drm_gem_object_put(buffer);
  return ret;
}
