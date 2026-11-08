use probe_api::{HssApi, HssBlock, HssCaps, Result};

use crate::ffi::{HssBlockDesc, HssCapsRaw};
use crate::jlink::{call_error, dll, JLink};

impl HssApi for JLink {
    fn hss_caps(&mut self) -> Result<HssCaps> {
        let mut raw = HssCapsRaw::default();
        let code = dll!(self, JLINK_HSS_GetCaps(&mut raw));
        if code < 0 {
            return Err(call_error("JLINK_HSS_GetCaps", code));
        }
        Ok(HssCaps {
            max_blocks: raw.words[0],
            max_freq_hz: raw.words[1],
            caps: raw.words[2],
            raw: raw.words,
        })
    }

    fn hss_start(&mut self, blocks: &[HssBlock], period_us: u32, flags: u32) -> Result<()> {
        let descriptors: Vec<HssBlockDesc> = blocks
            .iter()
            .map(|block| HssBlockDesc {
                addr: block.addr,
                num_bytes: block.num_bytes,
                flags: 0,
                reserved: 0,
            })
            .collect();
        let code = dll!(
            self,
            JLINK_HSS_Start(
                descriptors.as_ptr(),
                descriptors.len() as i32,
                period_us as i32,
                flags as i32,
            )
        );
        if code < 0 {
            return Err(call_error("JLINK_HSS_Start", code));
        }
        Ok(())
    }

    fn hss_read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let code = dll!(self, JLINK_HSS_Read(buf.as_mut_ptr(), buf.len() as u32));
        if code < 0 {
            return Err(call_error("JLINK_HSS_Read", code));
        }
        Ok(code as usize)
    }

    fn hss_stop(&mut self) -> Result<()> {
        let code = dll!(self, JLINK_HSS_Stop());
        if code < 0 {
            return Err(call_error("JLINK_HSS_Stop", code));
        }
        Ok(())
    }
}
