//! jarsuite: installs, updates and launches a suite of jarexec applications that
//! are released together because they share data.

pub mod assemble;
#[cfg(feature = "gui")]
pub mod gui;
pub mod install;
pub mod manifest;
pub mod run;
pub mod source;
#[cfg(test)]
mod test_util;
