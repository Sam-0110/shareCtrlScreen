//! The two media threads of a session: host capture → encode and viewer
//! decode → present. Both talk to the OS only through `platform_api`.

pub(crate) mod av1;
pub(crate) mod cursor;
pub(crate) mod host;
pub(crate) mod viewer;
