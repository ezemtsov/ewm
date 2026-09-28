pub mod foreign_toplevel;
pub mod gamma_control;
pub mod output_management;
pub mod screencopy;
pub mod virtual_pointer;
pub mod workspace;

/// Empty user-data for protocol impls.
///
/// Same as Smithay's `GlobalData` which we can't use due to coherence.
#[derive(Debug)]
pub struct EmptyData;
