use log::{error, info};
use rusb::{self, DeviceHandle, GlobalContext};
use std::{
  error::Error,
  mem::swap,
  ops::{Deref, DerefMut},
  time::Duration,
};

use crate::{
  shared::{
    utils::{Buffer, ShimError},
    worker::ThreadJob,
  },
  state::{SliderInput, SliderLights, SliderState},
};

use super::config::HardwareSpec;

type HidReadCallback = fn(&Buffer, &mut SliderInput) -> ();
type HidLedCallback = fn(&mut Buffer, &mut Buffer, &SliderLights) -> ();

enum WriteType {
  Bulk,
  Interrupt,
}

pub struct HidJob {
  state: SliderState,

  vid: u16,
  pid: u16,
  read_endpoint: u8,
  led_endpoint: u8,
  disable_air: bool,

  read_callback: HidReadCallback,
  read_buf: Buffer,
  last_read_buf: Buffer,

  led_write_type: WriteType,
  led_callback: HidLedCallback,
  led_buf: Buffer,
  led_buf_two: Buffer,

  handle: Option<DeviceHandle<GlobalContext>>,
}

impl HidJob {
  fn new(
    state: SliderState,
    vid: u16,
    pid: u16,
    read_endpoint: u8,
    led_endpoint: u8,
    disable_air: bool,
    read_callback: HidReadCallback,
    led_type: WriteType,
    led_callback: HidLedCallback,
  ) -> Self {
    Self {
      state,
      vid,
      pid,
      read_endpoint,
      led_endpoint,
      disable_air,
      read_callback,
      read_buf: Buffer::new(),
      last_read_buf: Buffer::new(),
      led_write_type: led_type,
      led_callback,
      led_buf: Buffer::new(),
      led_buf_two: Buffer::new(),
      handle: None,
    }
  }

  pub fn from_config(state: &SliderState, spec: &HardwareSpec, disable_air: &bool) -> Self {
    match spec {
      HardwareSpec::TasollerOne => Self::new(
        state.clone(),
        0x1ccf,
        0x2333,
        0x84,
        0x03,
        *disable_air,
        |buf, input| {
          if buf.len != 11 {
            return;
          }

          let bits: Vec<u8> = buf
            .data
            .iter()
            .flat_map(|x| (0..8).map(move |i| ((x) >> i) & 1))
            .collect();
          for i in 0..32 {
            input.ground[i] = bits[34 + i] * 255;
          }
          input.flip_vert();

          input.air.copy_from_slice(&bits[28..34]);
          input.extra[0..2].copy_from_slice(&bits[26..28]);
        },
        WriteType::Bulk,
        |buf, _, lights| {
          buf.len = 240;
          buf.data[0] = 'B' as u8;
          buf.data[1] = 'L' as u8;
          buf.data[2] = '\x00' as u8;
          for (buf_chunk, state_chunk) in buf.data[3..96]
            .chunks_mut(3)
            .take(31)
            .zip(lights.ground.chunks(3).rev())
          {
            buf_chunk[0] = state_chunk[1];
            buf_chunk[1] = state_chunk[0];
            buf_chunk[2] = state_chunk[2];
          }
          buf.data[96..240].fill(0);
        },
      ),
      HardwareSpec::TasollerTwo => Self::new(
        state.clone(),
        0x1ccf,
        0x2333,
        0x84,
        0x03,
        *disable_air,
        |buf, input| {
          if buf.len != 36 {
            return;
          }

          input.ground.copy_from_slice(&buf.data[4..36]);
          input.flip_vert();

          let bits: Vec<u8> = (0..8).map(|x| (buf.data[3] >> x) & 1).collect();
          input.air.copy_from_slice(&bits[0..6]);
          input.extra[0..2].copy_from_slice(&bits[6..8]);
        },
        WriteType::Bulk,
        |buf, _, lights| {
          buf.len = 240;
          buf.data[0] = 'B' as u8;
          buf.data[1] = 'L' as u8;
          buf.data[2] = '\x00' as u8;
          for (buf_chunk, state_chunk) in buf.data[3..96]
            .chunks_mut(3)
            .take(31)
            .zip(lights.ground.chunks(3).rev())
          {
            buf_chunk[0] = state_chunk[1];
            buf_chunk[1] = state_chunk[0];
            buf_chunk[2] = state_chunk[2];
          }

          for (buf_chunks, state_chunk) in buf.data[96..240].chunks_mut(24).zip(
            lights
              .air_left
              .chunks(3)
              .rev()
              .chain(lights.air_right.chunks(3)),
          ) {
            for idx in 0..8 {
              buf_chunks[0 + idx * 3] = state_chunk[1];
              buf_chunks[1 + idx * 3] = state_chunk[0];
              buf_chunks[2 + idx * 3] = state_chunk[2];
            }
          }
        },
      ),
      HardwareSpec::Yuancon => Self::new(
        state.clone(),
        0x1973,
        0x2001,
        0x81,
        0x02,
        *disable_air,
        |buf, input| {
          if buf.len != 34 && buf.len != 35 {
            return;
          }

          input.ground.copy_from_slice(&buf.data[2..34]);
          for i in 0..6 {
            input.air[i ^ 1] = (buf.data[0] >> i) & 1;
          }
          for i in 0..3 {
            input.extra[2 - i] = (buf.data[1] >> i) & 1;
          }
        },
        WriteType::Interrupt,
        |buf, _, lights| {
          buf.len = 31 * 2;
          for (buf_chunk, state_chunk) in buf
            .data
            .chunks_mut(2)
            .take(31)
            .zip(lights.ground.chunks(3).rev())
          {
            buf_chunk[0] = (state_chunk[0] << 3 & 0xe0) | (state_chunk[2] >> 3);
            buf_chunk[1] = (state_chunk[1] & 0xf8) | (state_chunk[0] >> 5);
          }
        },
      ),
      HardwareSpec::YuanconThree => Self::new(
        state.clone(),
        0x0518,
        0x2022,
        0x83,
        0x03,
        *disable_air,
        |buf, input| {
          if buf.len != 46 { // real length is 46 but last 12 bytes are unused
            return;
          }

          input.ground.copy_from_slice(&buf.data[2..34]);
          input.flip_vert();

          let bits: Vec<u8> = (0..8).map(|x| (buf.data[0] >> x) & 1).collect();
          for i in 0..6 {
            input.air[i ^ 1] = bits[i];
          }
          input.extra[0..2].copy_from_slice(&bits[6..8]);
        },
        WriteType::Interrupt,
        |buf, buf_two, lights| {
          buf.len = 61;
          buf.data[0] = 0;
          buf_two.len = 61;
          buf_two.data[0] = 1;

          for (buf_chunk, state_chunk) in buf.data[1..61]
              .chunks_mut(3)
              .zip(lights.ground.chunks(3).skip(11).take(20).rev())
          {
            buf_chunk[0] = state_chunk[0];
            buf_chunk[1] = state_chunk[1];
            buf_chunk[2] = state_chunk[2];
          }

          for (buf_chunk, state_chunk) in buf_two.data[1..34]
              .chunks_mut(3)
              .zip(lights.ground.chunks(3).take(11).rev())
          {
            buf_chunk[0] = state_chunk[0];
            buf_chunk[1] = state_chunk[1];
            buf_chunk[2] = state_chunk[2];
          }
        },
      ),
      HardwareSpec::Yubideck => Self::new(
        state.clone(),
        0x1973,
        0x2001,
        0x81, // Need to confirm
        0x02, // Need to confirm
        *disable_air,
        |buf, input| {
          if buf.len != 45 && buf.len != 46 {
            return;
          }

          input.ground.copy_from_slice(&buf.data[2..34]);
          input.flip_vert();
          for i in 0..6 {
            input.air[i ^ 1] = (buf.data[0] >> i) & 1;
          }
          for i in 0..3 {
            input.extra[2 - i] = (buf.data[1] >> i) & 1;
          }
        },
        WriteType::Interrupt,
        |buf, _, lights| {
          buf.len = 62;

          let lights_nibbles: Vec<u8> = lights
            .ground
            .chunks(3)
            .rev()
            .flat_map(|x| x.iter().map(|y| *y >> 4))
            .chain([
              lights.air_left[3] >> 4,
              lights.air_left[4] >> 4,
              lights.air_left[5] >> 4,
            ])
            .collect();

          for (buf_chunk, state_chunk) in buf
            .data
            .chunks_mut(3)
            .take(16)
            .zip(lights_nibbles.chunks(6))
          {
            buf_chunk[0] = (state_chunk[0]) | (state_chunk[1] << 4);
            buf_chunk[1] = (state_chunk[2]) | (state_chunk[3] << 4);
            buf_chunk[2] = (state_chunk[4]) | (state_chunk[5] << 4);
          }
        },
      ),
      HardwareSpec::YubideckThree => Self::new(
        state.clone(),
        0x1973,
        0x2001,
        0x81, // Need to confirm
        0x02, // Need to confirm
        *disable_air,
        |buf, input| {
          if buf.len != 45 && buf.len != 46 {
            return;
          }

          input.ground.copy_from_slice(&buf.data[2..34]);
          input.flip_vert();
          for i in 0..6 {
            input.air[i ^ 1] = (buf.data[0] >> i) & 1;
          }
          for i in 0..3 {
            input.extra[2 - i] = (buf.data[1] >> i) & 1;
          }
        },
        WriteType::Interrupt,
        |buf, buf_two, lights| {
          buf.len = 61;
          buf.data[0] = 0;
          buf_two.len = 61;
          buf_two.data[0] = 1;

          for (buf_chunk, state_chunk) in buf.data[1..61]
            .chunks_mut(3)
            .zip(lights.ground.chunks(3).skip(11).take(20).rev())
          {
            buf_chunk[0] = state_chunk[0];
            buf_chunk[1] = state_chunk[1];
            buf_chunk[2] = state_chunk[2];
          }

          for (buf_chunk, state_chunk) in buf_two.data[1..34]
            .chunks_mut(3)
            .zip(lights.ground.chunks(3).take(11).rev())
          {
            buf_chunk[0] = state_chunk[0];
            buf_chunk[1] = state_chunk[1];
            buf_chunk[2] = state_chunk[2];
          }

          buf_two.data[34..37].copy_from_slice(&lights.air_left[3..6]);
          buf_two.data[37..40].copy_from_slice(&lights.air_right[3..6]);
        },
      ),
    }
  }

  fn get_handle(&mut self) -> Result<(), Box<dyn Error>> {
    info!("Device finding vid {} pid {}", self.vid, self.pid);
    let handle = rusb::open_device_with_vid_pid(self.vid, self.pid);
    if handle.is_none() {
      error!("Device not found");
      return Err(Box::new(ShimError));
    }
    let mut handle = handle.unwrap();
    info!("Device found {:?}", handle);

    if handle.kernel_driver_active(0).unwrap_or(false) {
      info!("Device detaching kernel driver");
      handle.detach_kernel_driver(0)?;
    }
    info!("Device setting configuration");
    handle.set_active_configuration(1)?;

    // A bit janky but Laverita v3 seems to require interface 3
    info!("Device claiming interface");
    if self.vid == 0x0518 && self.pid == 0x2022 {
      handle.claim_interface(3)?;
    } else {
      handle.claim_interface(0)?;
    }

    self.handle = Some(handle);
    Ok(())
  }
}

const TIMEOUT: Duration = Duration::from_millis(20);

impl ThreadJob for HidJob {
  fn setup(&mut self) -> bool {
    match self.get_handle() {
      Ok(_) => {
        info!("Device OK");
        true
      }
      Err(e) => {
        error!("Device setup failed: {}", e);
        false
      }
    }
  }

  fn tick(&mut self) -> bool {
    // Input loop
    let handle = self.handle.as_mut().unwrap();
    let mut work = false;

    {
      let res = handle
        .read_interrupt(self.read_endpoint, &mut self.read_buf.data, TIMEOUT)
        .map_err(|e| {
          // debug!("Device read error {}", &e);
          e
        })
        .unwrap_or(0);
      self.read_buf.len = res;
      // debug!("{:?}", self.read_buf.slice());
      // if self.read_buf.len != 0 {
      if (self.read_buf.len != 0) && (self.read_buf.slice() != self.last_read_buf.slice()) {
        work = true;
        let mut input_handle = self.state.input.lock();
        (self.read_callback)(&self.read_buf, input_handle.deref_mut());

        if self.disable_air {
          input_handle.air.fill(0);
        }
        swap(&mut self.read_buf, &mut self.last_read_buf);
      }
    }

    // Led loop
    {
      {
        let mut lights_handle = self.state.lights.lock();
        if lights_handle.dirty {
          (self.led_callback)(
            &mut self.led_buf,
            &mut self.led_buf_two,
            lights_handle.deref(),
          );
          lights_handle.dirty = false;
        }
      }

      if self.led_buf.len != 0 {
        let res = (match self.led_write_type {
          WriteType::Bulk => handle.write_bulk(self.led_endpoint, self.led_buf.slice(), TIMEOUT),
          WriteType::Interrupt => {
            handle.write_interrupt(self.led_endpoint, &self.led_buf.slice(), TIMEOUT)
          }
        })
        .map_err(|e| {
          // debug!("Device write error {}", e);
          e
        })
        .unwrap_or(0);
        if res == self.led_buf.len + 1 {
          // work = true;
          self.led_buf.len = 0;
        }
      }

      if self.led_buf_two.len != 0 {
        let res = (match self.led_write_type {
          WriteType::Bulk => {
            handle.write_bulk(self.led_endpoint, self.led_buf_two.slice(), TIMEOUT)
          }
          WriteType::Interrupt => {
            handle.write_interrupt(self.led_endpoint, &self.led_buf_two.slice(), TIMEOUT)
          }
        })
        .map_err(|e| {
          // debug!("Device write error {}", e);
          e
        })
        .unwrap_or(0);
        if res == self.led_buf_two.len + 1 {
          // work = true;
          self.led_buf_two.len = 0;
        }
      }
    }

    work
  }
}

impl Drop for HidJob {
  fn drop(&mut self) {
    if let Some(handle) = self.handle.as_mut() {
      handle.release_interface(0).ok();
    }
  }
}
