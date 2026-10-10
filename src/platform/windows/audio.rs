use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{PCWSTR, Result as ComResult, implement};

pub struct DefaultRenderWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    _com: Apartment,
}

impl DefaultRenderWatch {
    /// `on_change` runs on a WASAPI worker thread: it must not block or call back into the audio APIs.
    pub fn new(on_change: impl Fn() + Send + Sync + 'static) -> Result<DefaultRenderWatch, String> {
        let com = Apartment::enter()?;
        // SAFETY: a documented CLSID, no aggregation, and this thread is in an apartment; the enumerator is `ThreadingModel=Both`.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("audio device enumerator: {e}"))?;
        let client: IMMNotificationClient = Notify {
            on_change: Box::new(on_change),
        }
        .into();
        // SAFETY: `client` is a live interface pointer this struct owns until `Drop` unregisters it.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .map_err(|e| format!("audio device notifications: {e}"))?;
        Ok(DefaultRenderWatch {
            enumerator,
            client,
            _com: com,
        })
    }
}

impl Drop for DefaultRenderWatch {
    fn drop(&mut self) {
        // SAFETY: unregisters the pointer `new` registered while it is alive; Windows delivers no callbacks after.
        if let Err(e) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        } {
            tracing::warn!(error = %e, "audio device notifications not unregistered");
        }
    }
}

pub(super) struct Apartment {
    leave: bool,
}

impl Apartment {
    pub(super) fn enter() -> Result<Apartment, String> {
        // SAFETY: initialises the calling thread's apartment; balanced in `Drop`.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // RPC_E_CHANGED_MODE: already in a multithreaded apartment, which the enumerator works in and is not ours to leave.
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Apartment { leave: false });
        }
        hr.ok().map_err(|e| format!("COM apartment: {e}"))?;
        Ok(Apartment { leave: true })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.leave {
            // SAFETY: balances the `CoInitializeEx` in `enter`, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[implement(IMMNotificationClient)]
struct Notify {
    on_change: Box<dyn Fn() + Send + Sync>,
}

impl IMMNotificationClient_Impl for Notify_Impl {
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> ComResult<()> {
        if flow == eRender && (role == eConsole || role == eMultimedia) {
            (self.on_change)();
        }
        Ok(())
    }

    fn OnDeviceRemoved(&self, id: &PCWSTR) -> ComResult<()> {
        if !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }

    fn OnDeviceStateChanged(&self, id: &PCWSTR, _state: DEVICE_STATE) -> ComResult<()> {
        if !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> ComResult<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> ComResult<()> {
        if *key == PKEY_AudioEngine_DeviceFormat && !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }
}

const CAPTURE_ID_PREFIX: &str = "{0.0.1.";

fn is_capture(id: &PCWSTR) -> bool {
    if id.is_null() {
        return false;
    }
    // SAFETY: Windows passes a NUL-terminated id that outlives the callback.
    let wide = unsafe { id.as_wide() };
    wide.len() >= CAPTURE_ID_PREFIX.len()
        && wide
            .iter()
            .zip(CAPTURE_ID_PREFIX.bytes())
            .all(|(&w, b)| w == u16::from(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn only_a_recognised_capture_id_is_screened_out() {
        let capture = wide("{0.0.1.00000000}.{04385bed-c11a-4133-ae99-9468e4b0a8de}");
        assert!(is_capture(&PCWSTR(capture.as_ptr())));

        for not_capture in [
            "{0.0.0.00000000}.{94d19b7c-5433-4505-bb22-50b69bb593ba}",
            "{0.0.0.00000000}.{f92bcf48-abec-4907-aea1-1082698e885c}",
            "",
            "{",
            "{0.0.1",
            "\\\\?\\SWD#MMDEVAPI#somethingelse",
        ] {
            let buf = wide(not_capture);
            assert!(
                !is_capture(&PCWSTR(buf.as_ptr())),
                "`{not_capture}` was screened out; only a known capture id may be"
            );
        }

        assert!(!is_capture(&PCWSTR::null()));
    }

    #[test]
    #[ignore]
    fn a_watch_registers_and_leaves_the_apartment_as_it_found_it() {
        let watch = DefaultRenderWatch::new(|| {}).expect("first registration");
        drop(watch);
        let again = DefaultRenderWatch::new(|| {}).expect("registration after a clean drop");
        let overlapping = DefaultRenderWatch::new(|| {}).expect("a second live registration");
        drop(again);
        drop(overlapping);
    }
}
