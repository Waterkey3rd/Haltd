use std::ffi::c_void;
use std::ptr::null_mut;

use probe_api::{Result, RttApi, RttChannel, RttDirection, RttStatus};

use crate::ffi::{
    RttBufDesc, RttStart, RttStatusRaw, RTT_CMD_GETDESC, RTT_CMD_GETNUMBUF, RTT_CMD_GETSTAT,
    RTT_CMD_START, RTT_CMD_STOP,
};
use crate::jlink::{c_buffer, call_error, dll, JLink};

fn direction_code(direction: RttDirection) -> u32 {
    match direction {
        RttDirection::Up => 0,
        RttDirection::Down => 1,
    }
}

impl RttApi for JLink {
    fn rtt_start(&mut self, control_block: Option<u32>) -> Result<()> {
        let mut start = RttStart {
            control_block: control_block.unwrap_or(0),
            ..RttStart::default()
        };
        let argument = if control_block.is_some() {
            (&mut start as *mut RttStart).cast::<c_void>()
        } else {
            null_mut()
        };
        let code = dll!(self, JLINK_RTTERMINAL_Control(RTT_CMD_START, argument));
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Control(START)", code));
        }
        Ok(())
    }

    fn rtt_stop(&mut self) -> Result<()> {
        let code = dll!(self, JLINK_RTTERMINAL_Control(RTT_CMD_STOP, null_mut()));
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Control(STOP)", code));
        }
        Ok(())
    }

    fn rtt_num_buffers(&mut self, dir: RttDirection) -> Result<Option<u32>> {
        let mut direction = direction_code(dir);
        let argument = (&mut direction as *mut u32).cast::<c_void>();
        let code = dll!(self, JLINK_RTTERMINAL_Control(RTT_CMD_GETNUMBUF, argument));
        // A negative result means the control block has not been located yet.
        Ok(u32::try_from(code).ok())
    }

    fn rtt_channel(&mut self, dir: RttDirection, index: u32) -> Result<RttChannel> {
        let mut desc = RttBufDesc {
            index: index as i32,
            direction: direction_code(dir),
            name: [0; 32],
            size: 0,
            flags: 0,
        };
        let argument = (&mut desc as *mut RttBufDesc).cast::<c_void>();
        let code = dll!(self, JLINK_RTTERMINAL_Control(RTT_CMD_GETDESC, argument));
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Control(GETDESC)", code));
        }
        Ok(RttChannel {
            index,
            direction: dir,
            name: c_buffer(&desc.name),
            size: desc.size,
            flags: desc.flags,
        })
    }

    fn rtt_status(&mut self) -> Result<RttStatus> {
        let mut raw = RttStatusRaw::default();
        let argument = (&mut raw as *mut RttStatusRaw).cast::<c_void>();
        let code = dll!(self, JLINK_RTTERMINAL_Control(RTT_CMD_GETSTAT, argument));
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Control(GETSTAT)", code));
        }
        Ok(RttStatus {
            bytes_transferred: raw.bytes_transferred,
            bytes_read: raw.bytes_read,
            host_overflows: raw.host_overflows,
            running: raw.running != 0,
            up_buffers: raw.up_buffers,
            down_buffers: raw.down_buffers,
        })
    }

    fn rtt_read(&mut self, channel: u32, buf: &mut [u8]) -> Result<usize> {
        let code = dll!(
            self,
            JLINK_RTTERMINAL_Read(channel, buf.as_mut_ptr(), buf.len() as u32)
        );
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Read", code));
        }
        Ok(code as usize)
    }

    fn rtt_write(&mut self, channel: u32, data: &[u8]) -> Result<usize> {
        let code = dll!(
            self,
            JLINK_RTTERMINAL_Write(channel, data.as_ptr(), data.len() as u32)
        );
        if code < 0 {
            return Err(call_error("JLINK_RTTERMINAL_Write", code));
        }
        Ok(code as usize)
    }
}
