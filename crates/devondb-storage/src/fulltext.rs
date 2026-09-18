//! Full-text retrieval core: tokenizer, inverted postings, and integer BM25.
//!
//! Design: `docs/FULLTEXT.md` (BINDING). This module is PURE: it has no
//! pager, catalog, I/O, budget, or engine-state dependency. The fixed-point
//! natural logarithm uses the range-reduced identity
//! `ln(x) = e*ln(2) + 2*(z + z^3/3 + z^5/5 + ...)`, where
//! `z = (m - 1)/(m + 1)` and `x = m*2^e`. The identity is NIST DLMF
//! 4.6.4; the Q32.32 `ln(2)` value and the
//! finite odd-denominator coefficient list are pinned below. No platform
//! floating-point or `libm` operation participates in scoring.
//!
//! Q32.32 values normally fit in `u64`. Two sites deliberately use `u128`:
//! document-length normalization can have an integer part as large as the
//! `u64` corpus row count, and a document can contribute as many distinct
//! terms as its `u32` token count. Those format bounds require up to 96 bits
//! for a normalized length and 71 bits for the final Q32.32 score. The wider
//! carriers retain 32 fractional bits; they do not change the fixed-point
//! scale.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::mem::size_of;

use thiserror::Error;

/// Maximum token length in bytes.
pub const MAX_TOKEN_BYTES: usize = 64;

/// Number of fractional bits in every BM25 score.
pub const SCORE_FRACTIONAL_BITS: u32 = 32;

const Q32_ONE: u128 = 1_u128 << SCORE_FRACTIONAL_BITS;
const Q32_HALF: u128 = Q32_ONE / 2;

// Frozen BM25 constants. k1 is the nearest Q32.32 representation of 6/5;
// b=3/4 is exact in binary fixed point.
const K1_Q32: u128 = 5_153_960_755;
const K1_PLUS_ONE_Q32: u128 = K1_Q32 + Q32_ONE;
const B_Q32: u128 = 3_221_225_472;
const ONE_MINUS_B_Q32: u128 = Q32_ONE - B_Q32;
// Premises used by the 96/97/98-bit normalization and 71-bit score proofs.
const _: () = assert!(SCORE_FRACTIONAL_BITS == 32);
const _: () = assert!(K1_Q32 > 0 && K1_Q32 < 2 * Q32_ONE);
const _: () = assert!(B_Q32 <= Q32_ONE);
const _: () = assert!(K1_Q32 * B_Q32 < Q32_ONE * Q32_ONE);
const _: () = assert!(K1_PLUS_ONE_Q32 * 46 < 128 * Q32_ONE);

// Nearest Q32.32 representation of ln(2). The remaining coefficients in the
// cited atanh series are the reciprocals of these pinned odd denominators.
const LN_2_Q32: u128 = 2_977_044_472;
const LN_SERIES_DENOMINATORS: [u128; 12] = [1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23];

/// One tokenizer output token, represented as its exact normalized bytes.
///
/// A byte representation is necessary because the frozen 64-byte truncation
/// may end between UTF-8 code-unit bytes. ASCII letters are folded in place;
/// non-ASCII bytes are retained exactly.
pub type Token = Vec<u8>;

/// A deterministic BM25 score with 32 fractional bits.
///
/// The wider carrier is required for the sum across all distinct terms in a
/// maximum-length format document. Compare scores as integers; conversion to
/// floating point is neither needed nor used by this module.
pub type Bm25Score = u128;

/// Errors returned while constructing a full-text index.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FullTextError {
    /// The caller refused a working-memory reservation before allocation.
    #[error("full-text working-memory reservation refused")]
    MemoryRefused,
    /// A row ordinal occurred more than once in one corpus.
    #[error("duplicate full-text row ordinal {0}")]
    DuplicateRowOrdinal(u64),
    /// A document's token count exceeded the format's u32 document bound.
    #[error("full-text document at row ordinal {row_ordinal} exceeds u32::MAX tokens")]
    DocumentTooLong {
        /// Ordinal of the rejected document.
        row_ordinal: u64,
    },
    /// A corpus collection could not be represented at the format bounds.
    #[error("full-text corpus exceeds the {boundary} bound")]
    CorpusTooLarge {
        /// Boundary whose integer representation overflowed.
        boundary: &'static str,
    },
}

/// One posting in a term's row-ordinal-ordered posting list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting {
    row_ordinal: u64,
    term_frequency: u32,
}

impl Posting {
    /// Returns the indexed row ordinal.
    #[must_use]
    pub fn row_ordinal(self) -> u64 {
        self.row_ordinal
    }

    /// Returns the term frequency in this row.
    #[must_use]
    pub fn term_frequency(self) -> u32 {
        self.term_frequency
    }
}

/// One row's token-count statistic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentLength {
    row_ordinal: u64,
    token_count: u32,
}

impl DocumentLength {
    /// Returns the indexed row ordinal.
    #[must_use]
    pub fn row_ordinal(self) -> u64 {
        self.row_ordinal
    }

    /// Returns the number of tokens in the row.
    #[must_use]
    pub fn token_count(self) -> u32 {
        self.token_count
    }
}

/// Corpus-wide statistics used by BM25.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusStats {
    document_count: u64,
    total_document_length: u128,
    average_document_length_q32: u64,
}

impl CorpusStats {
    /// Returns `N`, including documents containing zero tokens.
    #[must_use]
    pub fn document_count(self) -> u64 {
        self.document_count
    }

    /// Returns the sum of all per-document token counts.
    #[must_use]
    pub fn total_document_length(self) -> u128 {
        self.total_document_length
    }

    /// Returns average document length in Q32.32 form.
    #[must_use]
    pub fn average_document_length_q32(self) -> u64 {
        self.average_document_length_q32
    }
}

/// A dictionary term and its immutable posting list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermPostings {
    term: Box<[u8]>,
    document_frequency: u64,
    postings: Box<[Posting]>,
}

impl TermPostings {
    /// Returns the normalized term bytes.
    #[must_use]
    pub fn term(&self) -> &[u8] {
        &self.term
    }

    /// Returns the number of documents containing this term.
    #[must_use]
    pub fn document_frequency(&self) -> u64 {
        self.document_frequency
    }

    /// Returns postings ordered by row ordinal ascending.
    #[must_use]
    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }
}

/// Immutable in-memory full-text index for one `(table, column)` corpus.
///
/// Terms and rows are sorted so construction order cannot affect lookup,
/// scoring, or resident-size accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullTextIndex {
    terms: Box<[TermPostings]>,
    document_lengths: Box<[DocumentLength]>,
    stats: CorpusStats,
}

impl FullTextIndex {
    /// Builds an index from `(row_ordinal, text)` documents.
    ///
    /// Row ordinals may arrive in any order but must be unique.
    pub fn build<I, S>(documents: I) -> Result<Self, FullTextError>
    where
        I: IntoIterator<Item = (u64, S)>,
        S: AsRef<str>,
    {
        let mut builder = FullTextIndexBuilder::default();
        for (ordinal, text) in documents {
            builder.push(ordinal, text.as_ref())?;
        }
        Ok(builder.finish())
    }

    /// Returns the sorted term dictionary.
    #[must_use]
    pub fn terms(&self) -> &[TermPostings] {
        &self.terms
    }

    /// Returns per-row token counts ordered by row ordinal.
    #[must_use]
    pub fn document_lengths(&self) -> &[DocumentLength] {
        &self.document_lengths
    }

    /// Returns corpus-wide BM25 statistics.
    #[must_use]
    pub fn stats(&self) -> CorpusStats {
        self.stats
    }

    /// Looks up the posting list for exact normalized token bytes.
    #[must_use]
    pub fn postings(&self, term: &[u8]) -> Option<&TermPostings> {
        self.terms
            .binary_search_by(|entry| entry.term().cmp(term))
            .ok()
            .map(|index| &self.terms[index])
    }

    /// Returns one row's token count.
    #[must_use]
    pub fn document_length(&self, row_ordinal: u64) -> Option<u32> {
        self.document_lengths
            .binary_search_by_key(&row_ordinal, |entry| entry.row_ordinal)
            .ok()
            .map(|index| self.document_lengths[index].token_count)
    }

    /// Returns the exact requested heap bytes, excluding allocator metadata.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.resident_size_bytes() - size_of::<Self>()
    }

    /// Returns the exact requested bytes of all storage owned by this value.
    ///
    /// The count includes the inline index value (which lives in FT-2's Arc
    /// allocation), all boxed-slice element storage, every term byte, and
    /// every posting. Box allocator metadata and the Arc header are owned by
    /// their allocators/wrapper and therefore are not part of this value.
    #[must_use]
    pub fn resident_size_bytes(&self) -> usize {
        let mut bytes = size_of::<Self>();
        add_resident_bytes(&mut bytes, self.terms.len(), size_of::<TermPostings>());
        add_resident_bytes(
            &mut bytes,
            self.document_lengths.len(),
            size_of::<DocumentLength>(),
        );
        for entry in &self.terms {
            add_resident_bytes(&mut bytes, entry.term.len(), size_of::<u8>());
            add_resident_bytes(&mut bytes, entry.postings.len(), size_of::<Posting>());
        }
        bytes
    }
}

/// Prepared query terms and frozen corpus statistics for streaming scoring.
/// The executor charges `heap_bytes()` and owns its primary-key ordered top-k.
#[derive(Debug)]
pub struct FullTextQuery {
    terms: Vec<(Token, u64)>,
    stats: CorpusStats,
}

impl FullTextQuery {
    /// Prepares unique query terms, including terms absent from the base corpus.
    #[must_use]
    pub fn new(index: &FullTextIndex, text: &str) -> Self {
        let mut terms = tokenize(text);
        terms.sort_unstable();
        terms.dedup();
        Self {
            terms: terms
                .into_iter()
                .map(|term| {
                    let frequency = index
                        .postings(&term)
                        .map_or(0, TermPostings::document_frequency);
                    (term, frequency)
                })
                .collect(),
            stats: index.stats(),
        }
    }

    /// Bounds tokenizer and prepared-query allocations before construction.
    pub fn memory_bound(text: &str) -> Result<usize, FullTextError> {
        let mut tokens = 0_usize;
        visit_tokens(text, |_| {
            tokens = tokens.checked_add(1).ok_or(FullTextError::MemoryRefused)?;
            Ok(())
        })?;
        tokens
            .checked_mul(4 * (size_of::<(Token, u64)>() + MAX_TOKEN_BYTES))
            .ok_or(FullTextError::MemoryRefused)
    }

    /// Prepares query-sized statistics for a streaming corpus pass.
    /// Reserve `memory_bound(text)` before calling this constructor.
    #[must_use]
    pub fn empty_corpus(text: &str) -> Self {
        let mut terms = tokenize(text);
        terms.sort_unstable();
        terms.dedup();
        Self {
            terms: terms.into_iter().map(|term| (term, 0)).collect(),
            stats: CorpusStats {
                document_count: 0,
                total_document_length: 0,
                average_document_length_q32: 0,
            },
        }
    }

    /// Uses exactly the statistics retained by a checkpointed index.
    pub fn use_index(&mut self, index: &FullTextIndex) {
        self.stats = index.stats;
        for (term, frequency) in &mut self.terms {
            *frequency = index
                .postings(term)
                .map_or(0, TermPostings::document_frequency);
        }
    }

    /// Resets corpus counters, retaining the already charged query dictionary.
    pub fn clear_corpus(&mut self) {
        for (_, frequency) in &mut self.terms {
            *frequency = 0;
        }
        self.stats = CorpusStats {
            document_count: 0,
            total_document_length: 0,
            average_document_length_q32: 0,
        };
    }

    /// Adds one non-NULL document to N, total length and query-term frequencies.
    /// No memory is allocated; NULLs are excluded by the caller.
    pub fn observe_document(&mut self, text: &str) -> Result<(), FullTextError> {
        let mut length = 0_u32;
        visit_tokens(text, |_| {
            length = length
                .checked_add(1)
                .ok_or(FullTextError::DocumentTooLong {
                    row_ordinal: self.stats.document_count,
                })?;
            Ok(())
        })?;
        let count =
            self.stats
                .document_count
                .checked_add(1)
                .ok_or(FullTextError::CorpusTooLarge {
                    boundary: "document count",
                })?;
        let total = self.stats.total_document_length + u128::from(length);
        for (term, frequency) in &mut self.terms {
            let mut matched = false;
            visit_tokens(text, |token| {
                matched |= token == term.as_slice();
                Ok(())
            })?;
            *frequency += u64::from(matched);
        }
        self.stats = CorpusStats {
            document_count: count,
            total_document_length: total,
            average_document_length_q32: average_length_q32(total, count)?,
        };
        Ok(())
    }

    /// Whether the query has any recognized terms.
    #[must_use]
    pub fn has_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    /// Whether the corpus contains tokens and has a defined BM25 average.
    #[must_use]
    pub fn has_corpus(&self) -> bool {
        self.stats.total_document_length != 0
    }

    /// Returns exact heap bytes retained by the prepared query.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.terms.capacity() * size_of::<(Token, u64)>()
            + self
                .terms
                .iter()
                .map(|(term, _)| term.capacity())
                .sum::<usize>()
    }

    /// Whether a checkpointed row has any queried posting, even at score zero.
    #[must_use]
    pub fn matches_row(&self, index: &FullTextIndex, ordinal: u64) -> bool {
        self.terms.iter().any(|(term, _)| {
            index.postings(term).is_some_and(|entry| {
                entry
                    .postings()
                    .binary_search_by_key(&ordinal, |posting| posting.row_ordinal)
                    .is_ok()
            })
        })
    }

    /// Whether arbitrary text contains a query term; no scratch allocation.
    pub fn matches_text(&self, text: &str) -> Result<bool, FullTextError> {
        let mut matched = false;
        visit_tokens(text, |token| {
            matched |= self.terms.iter().any(|(term, _)| term.as_slice() == token);
            Ok(())
        })?;
        Ok(matched)
    }

    /// Scores a checkpointed ordinal without allocating score accumulators.
    #[must_use]
    pub fn score_row(&self, index: &FullTextIndex, ordinal: u64) -> Bm25Score {
        let Some(length) = index.document_length(ordinal) else {
            return 0;
        };
        self.terms.iter().fold(0, |score, (term, frequency)| {
            let count = index
                .postings(term)
                .and_then(|postings| {
                    postings
                        .postings()
                        .binary_search_by_key(&ordinal, |posting| posting.row_ordinal)
                        .ok()
                        .map(|position| postings.postings()[position].term_frequency)
                })
                .unwrap_or(0);
            add_score(score, self.contribution(*frequency, count, length))
        })
    }

    /// Scores overlay text against the frozen base statistics using stack scratch.
    /// Empty base corpora contribute zero until a corpus has been established.
    pub fn score_text(&self, text: &str) -> Result<Bm25Score, FullTextError> {
        let mut length = 0_u32;
        visit_tokens(text, |_| {
            length = length
                .checked_add(1)
                .ok_or(FullTextError::DocumentTooLong { row_ordinal: 0 })?;
            Ok(())
        })?;
        let mut score = 0;
        for (term, frequency) in &self.terms {
            let mut count = 0_u32;
            visit_tokens(text, |token| {
                count += u32::from(token == term.as_slice());
                Ok(())
            })?;
            score = add_score(score, self.contribution(*frequency, count, length));
        }
        Ok(score)
    }

    fn contribution(&self, frequency: u64, count: u32, length: u32) -> Bm25Score {
        if count == 0 || self.stats.document_count == 0 || self.stats.total_document_length == 0 {
            return 0;
        }
        bm25_term_score_q32(
            inverse_document_frequency_q32(self.stats.document_count, frequency),
            count,
            length,
            self.stats,
        )
    }
}

/// Builds an immutable full-text index from `(row_ordinal, text)` documents.
pub fn build_index<I, S>(documents: I) -> Result<FullTextIndex, FullTextError>
where
    I: IntoIterator<Item = (u64, S)>,
    S: AsRef<str>,
{
    FullTextIndex::build(documents)
}

/// Tokenizes text according to `docs/FULLTEXT.md` section 4.
///
/// ASCII letters and digits, plus every non-ASCII byte, continue a run.
/// ASCII punctuation and whitespace end it. Bytes after the 64-byte cap are
/// discarded until the separator, so one overlong run remains one token.
#[must_use]
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut token = Vec::new();
    let mut in_run = false;

    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || !byte.is_ascii() {
            in_run = true;
            if token.len() < MAX_TOKEN_BYTES {
                token.push(byte.to_ascii_lowercase());
            }
        } else if in_run {
            tokens.push(std::mem::take(&mut token));
            in_run = false;
        }
    }
    if in_run {
        tokens.push(token);
    }
    tokens
}

/// Scores matching rows and returns at most `k` results.
///
/// Results use an internal pre-order: score descending, row ordinal ascending.
/// TextScan must apply its normative primary-key tiebreak before truncation.
/// Repeated query terms are treated as one BM25 term, matching the standard
/// BM25 query-term sum without the optional `k3` query-frequency extension.
#[must_use]
pub fn query(index: &FullTextIndex, text: &str, k: usize) -> Vec<(u64, Bm25Score)> {
    if k == 0 || index.stats.document_count == 0 || index.stats.total_document_length == 0 {
        return Vec::new();
    }
    let mut query_terms = tokenize(text);
    query_terms.sort_unstable();
    query_terms.dedup();
    if query_terms.is_empty() {
        return Vec::new();
    }

    let mut cursors: Vec<_> = query_terms
        .iter()
        .filter_map(|term| index.postings(term).map(|postings| (postings, 0_usize)))
        .collect();
    let mut best = BinaryHeap::new();
    while let Some(row) = cursors
        .iter()
        .filter_map(|(term, pos)| term.postings().get(*pos).map(|posting| posting.row_ordinal))
        .min()
    {
        let mut score = 0;
        for (term, pos) in &mut cursors {
            if let Some(posting) = term.postings().get(*pos)
                && posting.row_ordinal == row
            {
                let idf = inverse_document_frequency_q32(
                    index.stats.document_count,
                    term.document_frequency,
                );
                score = add_score(
                    score,
                    bm25_term_score_q32(
                        idf,
                        posting.term_frequency,
                        index.document_length(row).unwrap_or(0),
                        index.stats,
                    ),
                );
                *pos += 1;
            }
        }
        let candidate = (Reverse(score), row);
        if best.len() < k {
            best.push(candidate);
        } else if best.peek().is_some_and(|worst| candidate < *worst) {
            best.pop();
            best.push(candidate);
        }
    }
    best.into_sorted_vec()
        .into_iter()
        .map(|(Reverse(score), row)| (row, score))
        .collect()
}

#[derive(Debug, Default)]
struct MutableTerm {
    term: Vec<u8>,
    postings: Vec<Posting>,
}

/// Incremental index construction with exactly measurable vector allocations.
#[derive(Debug, Default)]
pub struct FullTextIndexBuilder {
    terms: Vec<MutableTerm>,
    lengths: Vec<DocumentLength>,
    total: u128,
    allocated: usize,
}

impl FullTextIndexBuilder {
    /// Adds one uniquely numbered document; ordinals may arrive in any order.
    pub fn push(&mut self, row_ordinal: u64, text: &str) -> Result<(), FullTextError> {
        self.push_with_reservation(row_ordinal, text, |_| true)
    }

    /// Adds a document, reserving the total requested heap bytes before growth.
    /// The callback also sees released realloc capacity. On refusal, discard
    /// this builder; previously indexed documents may have partial new postings.
    pub fn push_with_reservation(
        &mut self,
        row_ordinal: u64,
        text: &str,
        mut reserve: impl FnMut(usize) -> bool,
    ) -> Result<(), FullTextError> {
        let position = match self
            .lengths
            .binary_search_by_key(&row_ordinal, |row| row.row_ordinal)
        {
            Ok(_) => return Err(FullTextError::DuplicateRowOrdinal(row_ordinal)),
            Err(position) => position,
        };
        let mut count = 0_u32;
        visit_tokens(text, |_| {
            count = count
                .checked_add(1)
                .ok_or(FullTextError::DocumentTooLong { row_ordinal })?;
            Ok(())
        })?;
        let total =
            self.total
                .checked_add(u128::from(count))
                .ok_or(FullTextError::CorpusTooLarge {
                    boundary: "total document length",
                })?;
        // Validate all fallible corpus bounds before modifying the builder.
        let documents = u64::try_from(self.lengths.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or(FullTextError::CorpusTooLarge {
                boundary: "u64 document count",
            })?;
        average_length_q32(total, documents)?;
        reserve_builder_vec(&mut self.lengths, &mut self.allocated, &mut reserve)?;
        visit_tokens(text, |token| {
            self.push_token(row_ordinal, token, &mut reserve)
        })?;
        self.lengths.insert(
            position,
            DocumentLength {
                row_ordinal,
                token_count: count,
            },
        );
        self.total = total;
        Ok(())
    }

    fn push_token(
        &mut self,
        row: u64,
        token: &[u8],
        reserve: &mut impl FnMut(usize) -> bool,
    ) -> Result<(), FullTextError> {
        let position = self
            .terms
            .binary_search_by(|entry| entry.term.as_slice().cmp(token));
        let term = match position {
            Ok(position) => &mut self.terms[position],
            Err(position) => {
                reserve_builder_vec(&mut self.terms, &mut self.allocated, reserve)?;
                let bytes = self.allocated + token.len();
                if !reserve(bytes) {
                    return Err(FullTextError::MemoryRefused);
                }
                self.terms.insert(
                    position,
                    MutableTerm {
                        term: token.to_vec(),
                        postings: Vec::new(),
                    },
                );
                self.allocated = bytes;
                &mut self.terms[position]
            }
        };
        match term
            .postings
            .binary_search_by_key(&row, |posting| posting.row_ordinal)
        {
            Ok(position) => term.postings[position].term_frequency += 1,
            Err(position) => {
                reserve_builder_vec(&mut term.postings, &mut self.allocated, reserve)?;
                term.postings.insert(
                    position,
                    Posting {
                        row_ordinal: row,
                        term_frequency: 1,
                    },
                );
            }
        }
        Ok(())
    }

    /// Returns exact heap allocations of the mutable structures, by capacity.
    #[must_use]
    pub fn resident_size_bytes(&self) -> usize {
        self.terms.capacity() * size_of::<MutableTerm>()
            + self.lengths.capacity() * size_of::<DocumentLength>()
            + self
                .terms
                .iter()
                .map(|entry| {
                    entry.term.capacity() + entry.postings.capacity() * size_of::<Posting>()
                })
                .sum::<usize>()
    }

    /// Bounds the peak while freezing old and new dictionary containers.
    #[must_use]
    pub fn finish_peak_bytes(&self) -> usize {
        self.resident_size_bytes()
            + self.terms.len() * size_of::<TermPostings>()
            + self.lengths.len() * size_of::<DocumentLength>()
            + self
                .terms
                .iter()
                .map(|term| term.term.len() + term.postings.len() * size_of::<Posting>())
                .max()
                .unwrap_or(0)
    }

    /// Freezes the corpus, releasing spare capacity without changing postings.
    #[must_use]
    pub fn finish(self) -> FullTextIndex {
        let count = self.lengths.len() as u64;
        let average = if count == 0 {
            0
        } else {
            ((self.total << SCORE_FRACTIONAL_BITS) / u128::from(count)) as u64
        };
        FullTextIndex {
            terms: self
                .terms
                .into_iter()
                .map(|term| TermPostings {
                    term: term.term.into_boxed_slice(),
                    document_frequency: term.postings.len() as u64,
                    postings: term.postings.into_boxed_slice(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            document_lengths: self.lengths.into_boxed_slice(),
            stats: CorpusStats {
                document_count: count,
                total_document_length: self.total,
                average_document_length_q32: average,
            },
        }
    }
}

fn reserve_builder_vec<T>(
    values: &mut Vec<T>,
    allocated: &mut usize,
    reserve: &mut impl FnMut(usize) -> bool,
) -> Result<(), FullTextError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let capacity = values.capacity().saturating_mul(2).max(4);
    let requested = capacity
        .checked_mul(size_of::<T>())
        .and_then(|bytes| allocated.checked_add(bytes))
        .ok_or(FullTextError::MemoryRefused)?;
    // Reserve the new allocation while the previous allocation still exists.
    if !reserve(requested) {
        return Err(FullTextError::MemoryRefused);
    }
    let old_bytes = values.capacity() * size_of::<T>();
    values
        .try_reserve_exact(capacity - values.len())
        .map_err(|_| FullTextError::MemoryRefused)?;
    *allocated = requested - old_bytes;
    if !reserve(*allocated) {
        return Err(FullTextError::MemoryRefused);
    }
    Ok(())
}

fn visit_tokens(
    text: &str,
    mut visit: impl FnMut(&[u8]) -> Result<(), FullTextError>,
) -> Result<(), FullTextError> {
    let mut token = [0_u8; MAX_TOKEN_BYTES];
    let mut length = 0;
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || !byte.is_ascii() {
            if length < MAX_TOKEN_BYTES {
                token[length] = byte.to_ascii_lowercase();
                length += 1;
            }
        } else if length != 0 {
            visit(&token[..length])?;
            length = 0;
        }
    }
    if length != 0 {
        visit(&token[..length])?;
    }
    Ok(())
}

fn average_length_q32(total: u128, count: u64) -> Result<u64, FullTextError> {
    if count == 0 {
        return Ok(0);
    }
    // total <= u64::MAX * u32::MAX < 2^96, so shifting by the 32 Q bits
    // stays below 2^128 at the format bounds.
    if total > u128::MAX >> SCORE_FRACTIONAL_BITS {
        return Err(FullTextError::CorpusTooLarge {
            boundary: "Q32.32 average document length",
        });
    }
    let shifted = total << SCORE_FRACTIONAL_BITS;
    let average = shifted / u128::from(count);
    u64::try_from(average).map_err(|_| FullTextError::CorpusTooLarge {
        boundary: "u64 Q32.32 average document length",
    })
}

fn inverse_document_frequency_q32(document_count: u64, document_frequency: u64) -> u128 {
    // Overlay-only query terms have df=0 in the checkpointed corpus.
    debug_assert!(document_frequency <= document_count);
    // BM25 IDF is ln(1 + (N-df+0.5)/(df+0.5)), algebraically equal to
    // ln(2*(N+1)/(2*df+1)). u128 is required because N+1 does not fit u64
    // when the format-level row count is u64::MAX; both doubled values fit
    // within 65 bits.
    let numerator = 2 * (u128::from(document_count) + 1);
    let denominator = 2 * u128::from(document_frequency) + 1;
    ln_ratio_q32(numerator, denominator).max(1)
}

fn bm25_term_score_q32(
    idf_q32: u128,
    term_frequency: u32,
    document_length: u32,
    stats: CorpusStats,
) -> u128 {
    let length_ratio = length_ratio_q32(document_length, stats);
    // Base rows have length_ratio <= N; overlay rows may reach dl*N.
    // The frozen k1*b<1 premise keeps their normalized denominator in u128.
    let normalized_length = ONE_MINUS_B_Q32 + multiply_q32(B_Q32, length_ratio);
    let frequency_q32 = u128::from(term_frequency) << SCORE_FRACTIONAL_BITS;
    // For base rows k1*normalized_length stays below 2^97. Even maximal
    // overlay dl*N leaves more than 2^64 headroom under frozen k1*b<1.
    let denominator = frequency_q32 + multiply_q32(K1_Q32, normalized_length);

    // tf is at most u32::MAX, so `frequency_q32` is below 2^64. Multiplying
    // by (k1+1) stays below 2^98. The denominator is positive because every
    // posting has tf>=1. The quotient is the Q32.32 saturation factor and is
    // at most k1+1.
    debug_assert!(term_frequency > 0);
    let weighted_frequency = checked_product(frequency_q32, K1_PLUS_ONE_Q32) / denominator;
    multiply_q32(idf_q32, weighted_frequency)
}

fn length_ratio_q32(document_length: u32, stats: CorpusStats) -> u128 {
    debug_assert!(stats.document_count > 0);
    debug_assert!(stats.total_document_length > 0);
    // dl*N is below 2^96 for u32 dl and u64 N. Appending 32 fractional bits
    // is therefore strictly below 2^128. The quotient may have a 64-bit
    // integer part (one non-empty row among u64::MAX rows), which is why this
    // Q32-scaled intermediate uses u128 rather than the normal u64 carrier.
    let numerator = u128::from(document_length) * u128::from(stats.document_count);
    debug_assert!(numerator <= u128::MAX >> SCORE_FRACTIONAL_BITS);
    (numerator << SCORE_FRACTIONAL_BITS) / stats.total_document_length
}

fn ln_ratio_q32(numerator: u128, denominator: u128) -> u128 {
    debug_assert!(numerator >= denominator);
    debug_assert!(denominator > 0);
    let mut scaled_denominator = denominator;
    let mut exponent = 0_u32;
    while scaled_denominator <= numerator / 2 {
        scaled_denominator *= 2;
        exponent += 1;
    }

    // Range reduction gives m in [1, 2), hence z in [0, 1/3). Both source
    // values are at most 65 bits for BM25 IDF, so the Q32 numerator fits u128.
    let difference = numerator - scaled_denominator;
    let sum = numerator + scaled_denominator;
    let z = ((difference << SCORE_FRACTIONAL_BITS) + sum / 2) / sum;
    let z_squared = multiply_q32(z, z);
    let mut power = z;
    let mut series = 0_u128;
    for coefficient_denominator in LN_SERIES_DENOMINATORS {
        series += power / coefficient_denominator;
        power = multiply_q32(power, z_squared);
    }
    let reduced_ln = 2 * series;
    u128::from(exponent) * LN_2_Q32 + reduced_ln
}

fn multiply_q32(left: u128, right: u128) -> u128 {
    // Splitting before multiplication also covers an overlay document longer
    // than the complete base corpus. Its length ratio can use all 128 bits;
    // the unsplit Q64.64 product would exceed u128 although the result fits.
    // Every call has a small left factor (<2^38), so the fractional product
    // and its rounding addition fit. k1*b<1 bounds the normalized result.
    let integer = checked_product(left, right >> SCORE_FRACTIONAL_BITS);
    let fractional = checked_product(left, right & (Q32_ONE - 1));
    add_score(integer, (fractional + Q32_HALF) >> SCORE_FRACTIONAL_BITS)
}

fn add_score(score: u128, contribution: u128) -> u128 {
    match score.checked_add(contribution) {
        Some(sum) => sum,
        None => {
            debug_assert!(false, "proved full-text score sum overflowed");
            u128::MAX
        }
    }
}

fn checked_product(left: u128, right: u128) -> u128 {
    match left.checked_mul(right) {
        Some(product) => product,
        None => {
            debug_assert!(false, "proved full-text fixed-point product overflowed");
            u128::MAX
        }
    }
}

fn add_resident_bytes(total: &mut usize, count: usize, width: usize) {
    let Some(allocation) = count.checked_mul(width) else {
        debug_assert!(false, "resident allocation width overflowed usize");
        *total = usize::MAX;
        return;
    };
    let Some(updated) = total.checked_add(allocation) else {
        // Simultaneously resident allocation payloads cannot exceed the
        // process address space. Keep release behavior deterministic if a
        // nonconforming allocator violates that invariant.
        debug_assert!(false, "resident allocation total overflowed usize");
        *total = usize::MAX;
        return;
    };
    *total = updated;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fulltext_tokenizer_folds_separates_and_caps_runs() {
        let overlong = "A".repeat(MAX_TOKEN_BYTES + 5);
        let text = format!("Hello,_WORLD 42 {overlong}!tail");
        let tokens = tokenize(&text);
        assert_eq!(tokens[0], b"hello");
        assert_eq!(tokens[1], b"world");
        assert_eq!(tokens[2], b"42");
        assert_eq!(tokens[3], vec![b'a'; MAX_TOKEN_BYTES]);
        assert_eq!(tokens[4], b"tail");
    }

    #[test]
    fn fulltext_tokenizer_preserves_non_ascii_bytes_exactly() {
        assert_eq!(
            tokenize("CAFÉ café"),
            vec![b"caf\xc3\x89".to_vec(), b"caf\xc3\xa9".to_vec()]
        );
        let capped = tokenize(&"é".repeat(33));
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].len(), MAX_TOKEN_BYTES);
        assert_eq!(capped[0], "é".repeat(32).into_bytes());
    }

    #[test]
    fn fulltext_fixed_point_ln_tracks_known_q32_values() {
        assert_eq!(ln_ratio_q32(1, 1), 0);
        assert_eq!(ln_ratio_q32(2, 1), LN_2_Q32);
        let ln_three = ln_ratio_q32(3, 1);
        let expected = 4_718_503_851_u128;
        assert!(ln_three.abs_diff(expected) <= 12);
    }

    #[test]
    fn fulltext_fixed_point_idf_has_a_positive_every_document_floor() {
        assert!(inverse_document_frequency_q32(1, 1) > 0);
        assert!(inverse_document_frequency_q32(u64::MAX, u64::MAX) > 0);
    }

    #[test]
    fn fulltext_overlay_length_at_format_bounds_does_not_overflow() {
        let stats = CorpusStats {
            document_count: u64::MAX,
            total_document_length: 1,
            average_document_length_q32: 0,
        };
        let score = bm25_term_score_q32(
            inverse_document_frequency_q32(u64::MAX, 0),
            u32::MAX,
            u32::MAX,
            stats,
        );
        assert!(score < Q32_ONE);
    }

    #[test]
    fn fulltext_index_rejects_duplicate_ordinals() {
        let error = FullTextIndex::build([(7, "first"), (7, "second")]);
        assert_eq!(error, Err(FullTextError::DuplicateRowOrdinal(7)));
    }
}
