//! A simulated BQ41Z50 on a fake I2C bus, for the host tests. It answers the SMBus transactions
//! the bq41z50 driver makes (see the driver's `Interface`):
//!
//!     register read    write_read([cmd])                   -> the register's bytes
//!     block read       write_read([cmd])                   -> count byte, then the data
//!     MAC read         write([0x44, 2, lo, hi]), then
//!                      write_read([0x44])                  -> count byte, lo, hi, then the data
//!     register write   write([cmd, data...])
//!     MAC write        write([0x44, len, lo, hi, data...])
//!
//! Anything it hasn't been given reads as zeros, so every accessor gets an answer. Every
//! transfer is logged, so tests can check what went out on the bus.
//!
//! `SimGauge` is a handle: clones share one gauge, so a test keeps a clone while the driver owns
//! the bus.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::vec::Vec;

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::{ErrorKind, ErrorType, I2c, NoAcknowledgeSource, Operation};

/// The gauge's SMBus address.
pub const ADDRESS: u8 = 0x0B;
/// ManufacturerBlockAccess, the SBS command MAC commands go through.
pub const MBA: u8 = 0x44;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BusError(pub ErrorKind);

impl embedded_hal_async::i2c::Error for BusError {
    fn kind(&self) -> ErrorKind {
        self.0
    }
}

/// One I2C transfer, as the gauge saw it.
#[derive(Debug, Clone, PartialEq)]
pub enum Transfer {
    Write(Vec<u8>),
    /// The bytes written, then how many were read back.
    WriteRead(Vec<u8>, usize),
    Read(usize),
}

#[derive(Clone, Default)]
pub struct SimGauge(Rc<RefCell<State>>);

#[derive(Default)]
struct State {
    /// Plain registers: their bytes in wire (little-endian) order.
    registers: HashMap<u8, Vec<u8>>,
    /// Block registers and strings: the data, without the count byte.
    blocks: HashMap<u8, Vec<u8>>,
    /// MAC command responses: the data, without the count byte and echoed code.
    macs: HashMap<u16, Vec<u8>>,
    refused_commands: HashSet<u8>,
    refused_macs: HashSet<u16>,
    /// The MAC code written to ManufacturerBlockAccess, waiting for its read.
    pending_mac: Option<u16>,
    /// Nothing ACKs, as if the gauge were unplugged or asleep.
    absent: bool,
    log: Vec<Transfer>,
}

impl SimGauge {
    pub fn set_word(&self, cmd: u8, value: u16) -> &Self {
        self.set_register(cmd, &value.to_le_bytes())
    }

    pub fn set_register(&self, cmd: u8, bytes: &[u8]) -> &Self {
        self.0.borrow_mut().registers.insert(cmd, bytes.to_vec());
        self
    }

    pub fn set_block(&self, cmd: u8, data: &[u8]) -> &Self {
        self.0.borrow_mut().blocks.insert(cmd, data.to_vec());
        self
    }

    pub fn set_mac(&self, code: u16, data: &[u8]) -> &Self {
        self.0.borrow_mut().macs.insert(code, data.to_vec());
        self
    }

    /// NACKs reads of `cmd`, the way a sealed gauge refuses some registers.
    pub fn refuse_command(&self, cmd: u8) -> &Self {
        self.0.borrow_mut().refused_commands.insert(cmd);
        self
    }

    /// NACKs the response read of MAC `code`.
    pub fn refuse_mac(&self, code: u16) -> &Self {
        self.0.borrow_mut().refused_macs.insert(code);
        self
    }

    /// Nothing ACKs while set, as if the gauge were unplugged or asleep.
    pub fn set_absent(&self, absent: bool) {
        self.0.borrow_mut().absent = absent;
    }

    /// Every transfer so far.
    pub fn log(&self) -> Vec<Transfer> {
        self.0.borrow().log.clone()
    }

    /// The MAC codes sent to ManufacturerBlockAccess without data (sends, and the first half of
    /// reads), in order. Access keys go out this way too.
    pub fn mac_codes_sent(&self) -> Vec<u16> {
        self.log()
            .iter()
            .filter_map(|t| match t {
                Transfer::Write(w) if w.len() == 4 && w[0] == MBA && w[1] == 2 => Some(u16::from_le_bytes([w[2], w[3]])),
                _ => None,
            })
            .collect()
    }
}

impl State {
    fn write(&mut self, bytes: &[u8]) -> Result<(), BusError> {
        let (&cmd, rest) = bytes.split_first().ok_or(BusError(ErrorKind::Other))?;
        if cmd == MBA {
            let (&len, payload) = rest.split_first().ok_or(BusError(ErrorKind::Other))?;
            assert_eq!(usize::from(len), payload.len(), "MAC write length byte");
            let code = u16::from_le_bytes([payload[0], payload[1]]);
            if payload.len() == 2 {
                self.pending_mac = Some(code);
            } else {
                self.macs.insert(code, payload[2..].to_vec());
            }
        } else {
            self.registers.insert(cmd, rest.to_vec());
        }
        Ok(())
    }

    fn write_read(&mut self, written: &[u8], read: &mut [u8]) -> Result<(), BusError> {
        let &[cmd] = written else { panic!("write_read of {written:02x?}: the driver only writes the command") };
        read.fill(0);
        if cmd == MBA {
            let code = self.pending_mac.take().expect("MAC read without a MAC command written first");
            if self.refused_macs.contains(&code) {
                return Err(BusError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data)));
            }
            let data = self.macs.get(&code).map_or(&[][..], Vec::as_slice);
            // The count covers the echoed code and the data bytes that follow it.
            read[0] = (read.len() - 1) as u8;
            read[1..3].copy_from_slice(&code.to_le_bytes());
            let copy = data.len().min(read.len() - 3);
            read[3..3 + copy].copy_from_slice(&data[..copy]);
            return Ok(());
        }
        if self.refused_commands.contains(&cmd) {
            return Err(BusError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Data)));
        }
        if let Some(data) = self.blocks.get(&cmd) {
            read[0] = data.len() as u8;
            let copy = data.len().min(read.len() - 1);
            read[1..1 + copy].copy_from_slice(&data[..copy]);
        } else if let Some(bytes) = self.registers.get(&cmd) {
            let copy = bytes.len().min(read.len());
            read[..copy].copy_from_slice(&bytes[..copy]);
        }
        Ok(())
    }
}

impl ErrorType for SimGauge {
    type Error = BusError;
}

impl I2c for SimGauge {
    async fn transaction(&mut self, address: u8, operations: &mut [Operation<'_>]) -> Result<(), Self::Error> {
        let mut gauge = self.0.borrow_mut();
        let transfer = match operations {
            [Operation::Write(w)] => Transfer::Write(w.to_vec()),
            [Operation::Write(w), Operation::Read(r)] => Transfer::WriteRead(w.to_vec(), r.len()),
            [Operation::Read(r)] => Transfer::Read(r.len()),
            _ => panic!("unexpected I2C transaction shape"),
        };
        gauge.log.push(transfer);
        if address != ADDRESS || gauge.absent {
            return Err(BusError(ErrorKind::NoAcknowledge(NoAcknowledgeSource::Address)));
        }
        match operations {
            [Operation::Write(w)] => gauge.write(w),
            [Operation::Write(w), Operation::Read(r)] => gauge.write_read(w, r),
            [Operation::Read(r)] => {
                r.fill(0);
                Ok(())
            }
            _ => unreachable!(),
        }
    }
}

/// A delay that returns at once and adds up how long it was asked to wait.
#[derive(Default)]
pub struct NoDelay {
    pub total_ns: u64,
}

impl DelayNs for NoDelay {
    async fn delay_ns(&mut self, ns: u32) {
        self.total_ns += u64::from(ns);
    }
}
