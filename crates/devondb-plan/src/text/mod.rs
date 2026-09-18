//! DevonPlan text form (`docs/PLAN_IR.md` § Text form, binding).
//!
//! The human surface of the IR: `lexer` tokenizes, `parser` builds the
//! same `Plan`/`StatementEnvelope` values the JSON form yields, and
//! `printer` emits canonical text (shortest spelling, defaults omitted).
//! Round-trip law: `parse(print(P)) == P` and `print(parse(T)) == T` for
//! canonical `T`.

pub mod lexer;
pub mod parser;
pub mod printer;
