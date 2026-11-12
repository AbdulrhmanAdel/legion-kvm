/* NVOS41_PARAMETERS + NvUnixEvent from the installed NVIDIA SDK. */
static long nvgpu_ioctl_event_data(struct nvgpu_fd *nfd, unsigned int cmd,
                                  void __user *uarg, unsigned int size) {
  u8 request[sizeof(struct nvgpu_ioctl_req) + 32] = {0};
  u8 response[sizeof(struct nvgpu_ioctl_resp) + 32] = {0};
  struct nvgpu_ioctl_req *req = (void *)request;
  struct nvgpu_ioctl_resp *resp = (void *)response;
  u8 *outer = request + sizeof(*req);
  u8 *returned = response + sizeof(*resp);
  __le64 pointer;
  __le32 status;
  int ret;

  if (size != 16)
    return -EINVAL;
  if (copy_from_user(outer, uarg, 16))
    return -EFAULT;
  memcpy(&pointer, outer, sizeof(pointer));
  if (!le64_to_cpu(pointer))
    return -EFAULT;
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(nfd->handle);
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(16);
  req->nested_offset = cpu_to_le32(16);
  req->nested_len = cpu_to_le32(16);
  /* Output-only record: the guest destination is never read or sent as data. */
  ret = nvgpu_send_recv(nfd->dev, request, sizeof(request), response, sizeof(response));
  if (ret < 0)
    return ret;
  ret = (s32)le32_to_cpu((__le32)resp->hdr.status);
  if (ret < 0)
    return ret;
  if (le32_to_cpu(resp->data_len) != 16 ||
      le32_to_cpu(resp->nested_len) != 16 || le32_to_cpu(resp->deep_len))
    return -EPROTO;
  /* Restore the caller's pointer even if a malformed reply changed it. */
  memcpy(returned, &pointer, sizeof(pointer));
  memcpy(&status, returned + 12, sizeof(status));
  if (!le32_to_cpu(status) &&
      copy_to_user((void __user *)(unsigned long)le64_to_cpu(pointer), returned + 16, 16))
    return -EFAULT;
  if (copy_to_user(uarg, returned, 16))
    return -EFAULT;
  return 0;
}
