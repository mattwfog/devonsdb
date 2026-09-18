use std::collections::{BTreeMap, BTreeSet};
use std::mem::{size_of, size_of_val};

use devondb_storage::fulltext::{FullTextIndex, build_index, query, tokenize};

fn oracle_rank(documents: &[(u64, String)], query_text: &str, k: usize) -> Vec<u64> {
    let tokenized = documents
        .iter()
        .map(|(row, text)| (*row, tokenize(text)))
        .collect::<Vec<_>>();
    let document_count = tokenized.len() as f64;
    let total_length = tokenized
        .iter()
        .map(|(_, tokens)| tokens.len())
        .sum::<usize>();
    if document_count == 0.0 || total_length == 0 {
        return Vec::new();
    }
    let average_length = total_length as f64 / document_count;
    let query_terms = tokenize(query_text).into_iter().collect::<BTreeSet<_>>();
    let mut scores = BTreeMap::<u64, f64>::new();

    for term in query_terms {
        let document_frequency = tokenized
            .iter()
            .filter(|(_, tokens)| tokens.contains(&term))
            .count() as f64;
        if document_frequency == 0.0 {
            continue;
        }
        let idf =
            (1.0 + (document_count - document_frequency + 0.5) / (document_frequency + 0.5)).ln();
        for (row, tokens) in &tokenized {
            let term_frequency = tokens.iter().filter(|token| **token == term).count() as f64;
            if term_frequency == 0.0 {
                continue;
            }
            let length_normalization = 1.0 - 0.75 + 0.75 * tokens.len() as f64 / average_length;
            let contribution = idf * (term_frequency * (1.2 + 1.0))
                / (term_frequency + 1.2 * length_normalization);
            *scores.entry(*row).or_default() += contribution;
        }
    }

    let mut ranked = scores.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(k);
    ranked.into_iter().map(|(row, _)| row).collect()
}

fn assert_oracle_rank(documents: &[(u64, String)], query_text: &str) {
    let index = build_index(
        documents
            .iter()
            .map(|(row_ordinal, text)| (*row_ordinal, text.as_str())),
    )
    .unwrap();
    let actual = query(&index, query_text, documents.len())
        .into_iter()
        .map(|(row_ordinal, _)| row_ordinal)
        .collect::<Vec<_>>();
    assert_eq!(actual, oracle_rank(documents, query_text, documents.len()));
}

#[test]
fn oracle_ranks_repeated_terms() {
    let documents = vec![
        (9, "rust rust rust database".to_owned()),
        (2, "rust database database database".to_owned()),
        (7, "quiet graph notes".to_owned()),
        (4, "rust graph database".to_owned()),
    ];
    assert_oracle_rank(&documents, "rust database");
    assert_oracle_rank(&documents, "database database rust");
}

#[test]
fn oracle_ranks_identical_documents_and_every_document_term() {
    let identical = vec![
        (30, "common same words".to_owned()),
        (4, "common same words".to_owned()),
        (18, "common same words".to_owned()),
    ];
    assert_oracle_rank(&identical, "same");
    let index = FullTextIndex::build(
        identical
            .iter()
            .map(|(row_ordinal, text)| (*row_ordinal, text.as_str())),
    )
    .unwrap();
    let ranked = query(&index, "common", identical.len());
    assert_eq!(
        ranked.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
        vec![4, 18, 30]
    );
    assert!(ranked.iter().all(|(_, score)| *score > 0));
}

#[test]
fn oracle_ranks_one_token_and_capped_token_documents() {
    let long_upper = "A".repeat(80);
    let capped = "a".repeat(64);
    let documents = vec![
        (5, "x".to_owned()),
        (1, "x x".to_owned()),
        (8, long_upper),
        (3, format!("{capped} tail")),
    ];
    assert_oracle_rank(&documents, "x");
    assert_oracle_rank(&documents, &format!("{capped}suffix"));
}

#[test]
fn oracle_preserves_non_ascii_bytes_without_folding() {
    let documents = vec![
        (1, "CAFÉ exact".to_owned()),
        (2, "café café exact".to_owned()),
        (3, "cafe exact".to_owned()),
    ];
    assert_oracle_rank(&documents, "café");
    assert_oracle_rank(&documents, "CAFÉ");

    let index = FullTextIndex::build(
        documents
            .iter()
            .map(|(row_ordinal, text)| (*row_ordinal, text.as_str())),
    )
    .unwrap();
    assert_eq!(
        query(&index, "café", documents.len())
            .into_iter()
            .map(|(row, _)| row)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        query(&index, "CAFÉ", documents.len())
            .into_iter()
            .map(|(row, _)| row)
            .collect::<Vec<_>>(),
        vec![1]
    );
}

#[test]
fn independently_built_indexes_and_queries_are_identical() {
    let documents = [
        (11, "red green green".to_owned()),
        (3, "green blue".to_owned()),
        (29, "blue red red".to_owned()),
        (5, String::new()),
    ];
    let build = || {
        FullTextIndex::build(
            documents
                .iter()
                .map(|(row_ordinal, text)| (*row_ordinal, text.as_str())),
        )
        .unwrap()
    };
    let first = build();
    let second = build();

    assert_eq!(first, second);
    assert_eq!(first.resident_size_bytes(), second.resident_size_bytes());
    assert_eq!(query(&first, "RED blue", 3), query(&second, "RED blue", 3));
}

#[test]
fn index_exposes_exact_stats_postings_and_resident_bytes() {
    let index = FullTextIndex::build([(20, "two two"), (4, "one"), (9, "")]).unwrap();
    assert_eq!(index.stats().document_count(), 3);
    assert_eq!(index.stats().total_document_length(), 3);
    assert_eq!(index.stats().average_document_length_q32(), 1_u64 << 32);
    assert_eq!(
        index
            .document_lengths()
            .iter()
            .map(|entry| (entry.row_ordinal(), entry.token_count()))
            .collect::<Vec<_>>(),
        vec![(4, 1), (9, 0), (20, 2)]
    );

    let two = index.postings(b"two").unwrap();
    assert_eq!(two.document_frequency(), 1);
    assert_eq!(two.postings()[0].row_ordinal(), 20);
    assert_eq!(two.postings()[0].term_frequency(), 2);

    let exact_bytes = size_of::<FullTextIndex>()
        + size_of_val(index.terms())
        + size_of_val(index.document_lengths())
        + index
            .terms()
            .iter()
            .map(|entry| entry.term().len() + size_of_val(entry.postings()))
            .sum::<usize>();
    assert_eq!(index.resident_size_bytes(), exact_bytes);
}

#[test]
fn zero_token_queries_return_no_rows() {
    let index = FullTextIndex::build([(1, "some text")]).unwrap();
    assert!(query(&index, "_ -- !", 10).is_empty());
    assert!(query(&index, "some", 0).is_empty());
}
