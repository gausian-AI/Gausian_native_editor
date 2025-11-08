use std::sync::Arc;

use eframe::wgpu;

use super::sync::GpuSyncController;

pub(crate) struct GpuContext<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    gpu_sync: Arc<GpuSyncController>,
}

impl<'a> GpuContext<'a> {
    pub(crate) fn new(
        render_state: &'a eframe::egui_wgpu::RenderState,
        gpu_sync: Arc<GpuSyncController>,
    ) -> Self {
        Self {
            device: render_state.device.as_ref(),
            queue: render_state.queue.as_ref(),
            gpu_sync,
        }
    }

    pub(crate) fn with_device<R>(&self, f: impl FnOnce(&wgpu::Device) -> R) -> R {
        f(self.device)
    }

    pub(crate) fn with_queue<R>(&self, f: impl FnOnce(&wgpu::Queue) -> R) -> R {
        f(self.queue)
    }

    pub(crate) fn gpu_sync(&self) -> &GpuSyncController {
        &self.gpu_sync
    }

    pub(crate) fn clone_sync(&self) -> Arc<GpuSyncController> {
        self.gpu_sync.clone()
    }
}
