//! DHCP message constants.

/// `flags` size in bytes.
pub const SIZE_FLAGS: usize = 2;

/// `client_hardware_address` size in bytes.
pub const SIZE_HARDWARE_ADDRESS: usize = 16;

/// The `server_name` field offset in bytes.
pub const OFFSET_SERVER_NAME: usize = 44;

/// `server_name` size in bytes.
pub const SIZE_SERVER_NAME: usize = 64;

/// The `boot_filename` field offset in bytes.
pub const OFFSET_BOOT_FILENAME: usize = OFFSET_SERVER_NAME + SIZE_SERVER_NAME;

/// `boot_filename` size in bytes.
pub const SIZE_BOOT_FILENAME: usize = 128;

/// DHCP options magic cookie offset in bytes.
pub const OFFSET_MAGIC_COOKIE: usize = OFFSET_SERVER_NAME + SIZE_SERVER_NAME + SIZE_BOOT_FILENAME;

/// DHCP options themselves offset in bytes.
pub const OFFSET_OPTIONS: usize = OFFSET_MAGIC_COOKIE + ::std::mem::size_of::<u32>();

/// 1 byte tag and 1 byte length before each DHCP option.
pub const SIZE_OPTION_PREFIX: usize = 2;

/// Only the highest bit of the `flags` field is used in DHCP.
pub const FLAG_BROADCAST: u16 = 0b1000000000000000;

/// The magic number before the DHCP options.
pub const MAGIC_COOKIE: u32 = 0x63825363;
