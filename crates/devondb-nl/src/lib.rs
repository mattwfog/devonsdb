//! devondb-nl — the deterministic natural-language intent compiler.
//!
//! Binding design: `docs/NL.md`. The deterministic compiler is the PRIMARY
//! surface — no language model runs in the core experience — and
//! [`IntentCompiler`] is the one seam every NL consumer wires through.

pub mod compiler;
pub mod ground;
pub mod normalize;
mod syntax;
pub mod template;
pub mod vocabulary;

pub use compiler::DeterministicCompiler;

use devondb::{Database, Plan, SchemaSummary};

/// The versioned identity of the NL vocabulary and template set.
///
/// Bumps on ANY vocabulary or template change (`docs/NL.md` § 5) so every
/// compile and every refusal is reproducible per version.
///
/// v6: reference-date-grounded Int64 epoch time phrases.
/// v7: aggregate question shapes — total/sum, average,
/// highest/lowest, and count per group (`docs/NL.md` § 17).
/// v8: similar-to templates — scalar-anchored KNN by example
/// (`docs/NL.md` § 18).
/// v9: negation/range templates, empty-result and
/// ambiguity behavior, and canonical-output corrections (`docs/NL.md` § 19).
/// v10: explicit grounded full-text search (`docs/NL.md` § 20).
pub const NL_VERSION: u32 = 10;

/// A natural-language front end that compiles a question into DevonPlan.
pub trait IntentCompiler {
    /// Compiles `question` against the committed catalog summary.
    ///
    /// The determinism law (`docs/NL.md` § 1): the same question, schema,
    /// and [`NL_VERSION`] produce an identical outcome, forever.
    fn compile(&self, question: &str, schema: &SchemaSummary) -> Compiled;

    /// Compiles with a caller-supplied UTC reference date.
    ///
    /// `reference_date` is an `i64` Unix epoch-second value at UTC midnight
    /// (therefore an exact multiple of 86,400). Implementations that do not
    /// use dates retain their existing behavior through this default. The
    /// deterministic compiler uses the value only to freeze time-phrase
    /// bounds into the returned plan; it never reads a clock.
    fn compile_at(&self, question: &str, schema: &SchemaSummary, reference_date: i64) -> Compiled {
        let _ = reference_date;
        self.compile(question, schema)
    }

    /// Compiles with access to committed rows for named-place grounding.
    ///
    /// Implementations that do not support row-backed grounding retain the
    /// schema-only behavior. The deterministic compiler overrides this so a
    /// unique Locatable entity becomes a GeoPoint literal in the plan.
    fn compile_with_database(&self, question: &str, database: &mut Database) -> Compiled {
        self.compile(question, &database.schema_summary())
    }

    /// Compiles with row-backed grounding and a caller-supplied UTC
    /// reference date.
    ///
    /// The default preserves schema-only behavior for implementations that
    /// do not override row-backed compilation. The deterministic compiler
    /// combines both inputs so time-phrase bounds are frozen while entity
    /// values resolve against committed rows.
    fn compile_at_with_database(
        &self,
        question: &str,
        database: &mut Database,
        reference_date: i64,
    ) -> Compiled {
        let schema = database.schema_summary();
        self.compile_at(question, &schema, reference_date)
    }

    /// Compiles a PK-addressed natural-language mutation.
    ///
    /// This additive seam defaults to an all-ungrounded refusal so existing
    /// and non-deterministic compilers opt out without changing [`Compiled`]
    /// or query compilation.
    fn compile_statement(&self, input: &str, _schema: &SchemaSummary) -> CompiledStatement {
        CompiledStatement::NoParse(compiler::statement_opt_out(input))
    }
}

/// The outcome of one compile: exactly one plan, or a structured refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum Compiled {
    /// The single deterministic plan for the question.
    Plan(Plan),
    /// The question did not fully ground; never a guess (`docs/NL.md` § 2).
    NoParse(NoParse),
}

/// The outcome of statement compilation: one PK-addressed statement or a
/// structured refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledStatement {
    /// The single deterministic PK-addressed statement.
    Statement(devondb::Statement),
    /// The input did not fully ground to an allowed statement template.
    NoParse(NoParse),
}

/// A structured refusal: what grounded, what did not, and the nearest
/// working phrasings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoParse {
    /// Tokens that grounded, with what they grounded to.
    pub recognized: Vec<Grounded>,
    /// Tokens that failed to ground, each with its nearest-name suggestion.
    pub unrecognized: Vec<Ungrounded>,
    /// Up to three nearest templates, instantiated with grounded slots,
    /// ranked by grounded-slot count then template order (`docs/NL.md` § 7).
    pub nearest: Vec<TemplateHint>,
}

/// One token that grounded during compilation.
#[derive(Debug, Clone, PartialEq)]
pub struct Grounded {
    /// The token as the user typed it.
    pub token: String,
    /// What it grounded to — a vocabulary word, a catalog name in display
    /// spelling, or a literal.
    pub target: String,
}

/// One token that failed to ground.
#[derive(Debug, Clone, PartialEq)]
pub struct Ungrounded {
    /// The token as the user typed it.
    pub token: String,
    /// The nearest catalog or vocabulary name when one is close enough
    /// (`devondb::did_you_mean`).
    pub suggestion: Option<String>,
}

/// One nearest-template hint in a refusal.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateHint {
    /// The template's example phrasing with the grounded slots filled in.
    pub example: String,
}
