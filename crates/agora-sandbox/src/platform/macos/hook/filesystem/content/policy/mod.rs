mod io;
mod mapping;

pub(crate) use io::{ContentIoOffset, managed_read_io, managed_seek_io, managed_write_io};
#[cfg(test)]
pub(crate) use io::{READ_AHEAD_MAX_BYTES, read_materialization_length};
pub(super) use io::{ReadOperations, WriteOperations};
pub(crate) use io::{errno_error, positional_reservation_range};
pub(super) use mapping::prepare_mapping;

#[cfg(test)]
mod tests;
