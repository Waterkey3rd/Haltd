//! ELF and DWARF handling (DESIGN §5.3).
//!
//! [`SymbolDb::load`] memory-maps the ELF and builds, or loads from disk, a
//! compact name index. Everything else (type trees, values) is parsed from the
//! DWARF on first use.

mod cache;
mod db;
mod expr;
mod format;
mod index;
mod lines;
mod loc;
pub mod paths;
mod scalar;
mod scope;
mod types;
mod unwind;

pub use db::{Function, GlobalCompletion, LoadOptions, LoadStats, SymbolDb, Variable};
pub use expr::{parse, BinaryOp, CastType, Evaluated, Expr, Place, UnaryOp, VirtualMembers};
pub use format::{FormatOptions, ReadResult};
pub use lines::{ResolvedLine, SourceLocation};
pub use loc::{FrameRegisters, Located};
pub use scalar::{Number, Scalar, ScalarKind};
pub use scope::{LocalVar, ScopeFrame};
pub use types::{BaseEncoding, BitField, Member, Type, TypeKind, TypeRef};
pub use unwind::{exception_frame_size, CoreRegisters, FrameKind, StackFrame, UnwindStopReason};

/// Target memory as the evaluator sees it.
pub trait MemoryReader {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> std::result::Result<(), String>;
}

pub type Result<T, E = SymbolError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid ELF: {0}")]
    Object(String),
    #[error("invalid DWARF: {0}")]
    Dwarf(#[from] gimli::Error),
    #[error("cannot parse expression: {0}")]
    Parse(String),
    #[error("no symbol named {0:?}")]
    NotFound(String),
    #[error("{name:?} is ambiguous: {candidates}")]
    Ambiguous { name: String, candidates: String },
    #[error("{0}")]
    Type(String),
    #[error("cannot read target memory: {0}")]
    Memory(String),
}
