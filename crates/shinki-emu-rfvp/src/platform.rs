use rfvp::app::embedded::session::{Platform, PointerRequest};
use shinki_emu_abi::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct HostWindow {
    pub service: WindowService,
    pub cancellation: CancellationService,
    pub session_epoch: u64,
    pub next_request: AtomicU64,
}
// HostServices explicitly permits native worker requests and concurrent cancellation;
// the host owns callback storage until close returns and all workers are joined.
unsafe impl Send for HostWindow {}
unsafe impl Sync for HostWindow {}

impl HostWindow {
    fn request(&self, kind: u32, flags: u32, context_epoch: u64) -> anyhow::Result<WindowRequest> {
        let request_id = self
            .next_request
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("RFVP_WINDOW_REQUEST_EXHAUSTED"))?;
        Ok(WindowRequest {
            session_epoch: self.session_epoch,
            context_epoch,
            request_id,
            kind,
            flags,
            text: Bytes::EMPTY,
            pixels: Bytes::EMPTY,
            width: 0,
            height: 0,
            x: 0,
            y: 0,
            autohide_ms: 0,
            buttons: std::ptr::null(),
            button_count: 0,
            caption: Bytes::EMPTY,
            initial_text: Bytes::EMPTY,
        })
    }

    fn dispatch(&self, request: &WindowRequest) -> anyhow::Result<WindowResult> {
        let call = self
            .service
            .request
            .ok_or_else(|| anyhow::anyhow!("RFVP_WINDOW_SERVICE_UNAVAILABLE"))?;
        let mut result = WindowResult {
            selected_button: 0,
            reserved: 0,
            valid_fields: 0,
            fullscreen: 0,
            width: 0,
            height: 0,
            x: 0,
            y: 0,
            input: TextBuffer {
                data: std::ptr::null_mut(),
                capacity: 0,
                len: 0,
            },
            visible: 0,
            minimized: 0,
        };
        let mut error = Error::default();
        let status = unsafe { call(self.service.context, request, &mut result, &mut error) };
        if status == CANCELLED {
            anyhow::bail!("RFVP_SESSION_CANCELLED");
        }
        if status != OK {
            let code =
                std::str::from_utf8(&error.code[..(error.code_len as usize).min(error.code.len())])
                    .unwrap_or("RFVP_WINDOW_REQUEST_FAILED");
            let message = std::str::from_utf8(
                &error.message[..(error.message_len as usize).min(error.message.len())],
            )
            .unwrap_or("invalid host error encoding");
            anyhow::bail!("{code}: {message} (status {status})");
        }
        Ok(result)
    }
}

impl Platform for HostWindow {
    fn window_mode(
        &self,
        mode: i32,
        logical: (u32, u32),
        context_epoch: u64,
    ) -> anyhow::Result<()> {
        let flags = match mode {
            0 => FULLSCREEN_WINDOWED,
            1 => FULLSCREEN_BORDERLESS,
            2 => FULLSCREEN_EXCLUSIVE,
            _ => anyhow::bail!("RFVP_WINDOW_MODE_INVALID"),
        };
        let mut request = self.request(WINDOW_FULLSCREEN, flags, context_epoch)?;
        request.width = logical.0;
        request.height = logical.1;
        let result = self.dispatch(&request)?;
        anyhow::ensure!(
            result.valid_fields & WINDOW_RESULT_MODE != 0 && result.fullscreen == flags,
            "RFVP_WINDOW_MODE_NOT_APPLIED"
        );
        Ok(())
    }

    fn pointer(&self, pointer: PointerRequest, context_epoch: u64) -> anyhow::Result<()> {
        let mut flags = 0;
        if pointer.position.is_some() {
            flags |= POINTER_SET_POSITION;
        }
        if let Some(visible) = pointer.visible {
            flags |= POINTER_SET_VISIBILITY;
            if visible {
                flags |= POINTER_VISIBLE;
            }
        }
        let mut request = self.request(WINDOW_POINTER, flags, context_epoch)?;
        if let Some((x, y)) = pointer.position {
            request.x = x;
            request.y = y;
        }
        self.dispatch(&request)?;
        Ok(())
    }

    fn invalidate(&self, context_epoch: u64) {
        if let Some(invalidate) = self.service.invalidate {
            unsafe { invalidate(self.service.context, self.session_epoch, context_epoch) };
        }
    }

    fn is_cancelled(&self) -> bool {
        // Validated as required before creating the native owner.
        let callback = self
            .cancellation
            .is_cancelled
            .expect("validated cancellation service");
        unsafe { callback(self.cancellation.context, self.session_epoch) != 0 }
    }
}
