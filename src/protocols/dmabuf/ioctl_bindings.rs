use std::{
	ffi::c_char,
	os::fd::{FromRawFd as _, OwnedFd, RawFd},
};

use rustix::ioctl::{
	Ioctl,
	opcode::{read_write, write},
};

bitflags::bitflags! {
	#[repr(transparent)]
	pub struct DmabufSyncFlags: u32 {
		const DMA_BUF_SYNC_READ = 1 << 0;
		const DMA_BUF_SYNC_WRITE = 1 << 1;
		const DMA_BUF_SYNC_RW = Self::DMA_BUF_SYNC_READ.bits() | Self::DMA_BUF_SYNC_WRITE.bits();
		// const DMA_BUF_SYNC_START = 0 << 2;
		// const DMA_BUF_SYNC_END = 1 << 2;
		// const DMA_BUF_SYNC_VALID_FLAGS_MASK = Self::DMA_BUF_SYNC_RW.bits() | Self::DMA_BUF_SYNC_END.bits();

		const _ = !0;
	}
}

pub const DMABUF_IOCTL_BASE: u8 = b'b';

#[repr(C)]
pub struct DmabufExportSyncFile {
	pub flags: DmabufSyncFlags,
	pub fd: RawFd,
}
unsafe impl Ioctl for DmabufExportSyncFile {
	type Output = OwnedFd;

	const IS_MUTATING: bool = true;

	fn opcode(&self) -> rustix::ioctl::Opcode {
		read_write::<Self>(DMABUF_IOCTL_BASE, 2)
	}

	fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
		self as *mut _ as *mut _
	}

	unsafe fn output_from_ptr(
		_out: rustix::ioctl::IoctlOutput,
		extract_output: *mut rustix::ffi::c_void,
	) -> rustix::io::Result<Self::Output> {
		let ptr = extract_output as *mut Self;
		let v = unsafe { &(*ptr) };

		Ok(unsafe { OwnedFd::from_raw_fd(v.fd) })
	}
}
#[repr(C)]
pub struct DmabufImportSyncFile {
	pub flags: DmabufSyncFlags,
	pub fd: RawFd,
}
unsafe impl Ioctl for DmabufImportSyncFile {
	type Output = ();

	const IS_MUTATING: bool = true;

	fn opcode(&self) -> rustix::ioctl::Opcode {
		write::<Self>(DMABUF_IOCTL_BASE, 3)
	}

	fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
		self as *mut _ as *mut _
	}

	unsafe fn output_from_ptr(
		_out: rustix::ioctl::IoctlOutput,
		_extract_output: *mut rustix::ffi::c_void,
	) -> rustix::io::Result<Self::Output> {
		Ok(())
	}
}

//
// SYNCFILE BEGIN
//

#[repr(C)]
pub struct SyncFileMerge {
	pub name: [c_char; 32],
	pub fd2: RawFd,
	pub fence: RawFd,
	// i think there are no flags defined?
	pub flags: u32,
	pub _pad: u32,
}
unsafe impl Ioctl for SyncFileMerge {
	type Output = OwnedFd;

	const IS_MUTATING: bool = true;

	fn opcode(&self) -> rustix::ioctl::Opcode {
		read_write::<Self>(b'>', 3)
	}

	fn as_ptr(&mut self) -> *mut rustix::ffi::c_void {
		self as *mut _ as *mut _
	}

	unsafe fn output_from_ptr(
		_out: rustix::ioctl::IoctlOutput,
		extract_output: *mut rustix::ffi::c_void,
	) -> rustix::io::Result<Self::Output> {
		let ptr = extract_output as *mut Self;
		let v = unsafe { &(*ptr) };

		Ok(unsafe { OwnedFd::from_raw_fd(v.fence) })
	}
}
