//! Xbox Series shaders to host shaders.
//!
//! An Xbox Series shader is a standard DXBC container ([`dxbc`]). Its `DXIL`
//! part ([`dxil`]) may carry Xbox additions around the LLVM bitcode: an
//! `XDAT` table before it indexing blobs after it, among them `xbop`
//! (compiler options) and the zlib-compressed GFX10 code. The bitcode may use
//! console-only intrinsics, so the GFX10 code is translated in general.

pub mod bvh;
pub mod dxbc;
pub mod dxil;
pub mod emit;
pub mod gfx10;
pub mod io;
pub mod ir;
pub mod native;
pub mod resource;
pub mod spill;
pub mod structure;

/// The version of what the translator makes: the shader cache keys every
/// translation on it (with the switches and host facts of
/// [`emit::translation_fingerprint`]), no longer on a hash of the
/// translator's source, so a rebuild that changes nothing it puts out keeps
/// every cached translation instead of making a new set of them.
///
/// **Bump it with every change that changes what a shader translates to**:
/// its DXIL, what the host binds for it ([`emit::HostShader`]), its wave
/// size, or whether it translates at all. Without the bump a run is served
/// the earlier build's translations. Changes that leave every translation as
/// it was (speed, diagnostics, tests, comments) leave it. When in doubt,
/// bump; `VICTORY_SHADER_KEY=source` keys by the source as before, for work
/// on the translator itself.
pub const TRANSLATOR_VERSION: u32 = 2;

pub use dxbc::{Container, FourCC, Part};
pub use dxil::{DxilProgram, ShaderKind, XboxExtras};
pub use native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, UsedResource, native_programs,
};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShaderError {
    #[error("not a DXBC container")]
    NotDxbc,
    #[error("malformed shader: {0}")]
    Malformed(&'static str),
    #[error("GFX10 code at {offset:#x}: {reason}")]
    Isa { offset: usize, reason: &'static str },
    /// Valid code the translator cannot handle yet: an opcode name or a construct.
    #[error("GFX10 code at {offset:#x}: {what} is not supported")]
    Unsupported { offset: usize, what: &'static str },
}
