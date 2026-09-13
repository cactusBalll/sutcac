//! Lexical lane: Tantivy BM25 index with the custom `code` tokenizer
//! (design §4.4). Consistency rule: the same [`crate::tokenizer::CODE_TOKENIZER_NAME`]
//! registration backs indexing and query parsing.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::query::QueryParser;
use tantivy::query::TermQuery;
use tantivy::schema::Value;
use tantivy::schema::{
    INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term};

use crate::error::{RagError, RagResult};
use crate::tokenizer::{CODE_TOKENIZER_NAME, CodeTokenizer};

/// One stored BM25 row.
#[derive(Debug, Clone)]
pub struct LexRow {
    pub doc_id: u64,
    pub text: String,
    pub path: String,
    pub lang: String,
}

/// Tantivy BM25 index over chunk texts.
pub struct LexicalIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    f_text: tantivy::schema::Field,
    f_path: tantivy::schema::Field,
    f_lang: tantivy::schema::Field,
    f_doc_id: tantivy::schema::Field,
}

impl LexicalIndex {
    /// Open or create the Tantivy index directory. The `code` tokenizer is
    /// (re-)registered on the reopened index as well.
    pub fn open(dir: &Path) -> RagResult<Self> {
        std::fs::create_dir_all(dir)?;
        let mut sb = Schema::builder();
        let text_options = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CODE_TOKENIZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let f_text = sb.add_text_field("text", text_options);
        let f_path = sb.add_text_field("path", STRING | STORED);
        let f_lang = sb.add_text_field("lang", STRING | STORED);
        let f_doc_id = sb.add_u64_field("doc_id", INDEXED | STORED);
        let schema = sb.build();

        let index = match Index::open_in_dir(dir) {
            Ok(index) => index,
            Err(_) => Index::create_in_dir(dir, schema.clone())
                .map_err(|e| RagError::Index(format!("tantivy create: {}", e)))?,
        };
        index
            .tokenizers()
            .register(CODE_TOKENIZER_NAME, CodeTokenizer::default());
        let reader = index
            .reader()
            .map_err(|e| RagError::Index(format!("tantivy reader: {}", e)))?;
        let writer = index
            .writer(50_000_000)
            .map_err(|e| RagError::Index(format!("tantivy writer: {}", e)))?;
        Ok(Self {
            index,
            reader,
            writer,
            f_text,
            f_path,
            f_lang,
            f_doc_id,
        })
    }

    /// Write one document row (uncommitted).
    pub fn add_row(&mut self, doc_id: u64, text: &str, path: &str, lang: &str) -> RagResult<()> {
        let doc = tantivy::doc!(
            self.f_text => text.to_string(),
            self.f_path => path.to_string(),
            self.f_lang => lang.to_string(),
            self.f_doc_id => doc_id,
        );
        self.writer
            .add_document(doc)
            .map(|_| ())
            .map_err(|e| RagError::Index(format!("tantivy add: {}", e)))
    }

    /// Physical delete of the given doc ids (logical deletes live in the
    /// mapping layer; both run together).
    pub fn delete_rows(&mut self, doc_ids: &[u64]) -> RagResult<()> {
        for id in doc_ids {
            self.writer
                .delete_term(Term::from_field_u64(self.f_doc_id, *id));
        }
        self.writer
            .commit()
            .map(|_| ())
            .map_err(|e| RagError::Index(format!("tantivy commit(del): {}", e)))?;
        self.reader
            .reload()
            .map_err(|e| RagError::Index(format!("tantivy reload: {}", e)))
    }

    /// Commit pending writes.
    pub fn commit(&mut self) -> RagResult<()> {
        self.writer
            .commit()
            .map(|_| ())
            .map_err(|e| RagError::Index(format!("tantivy commit: {}", e)))?;
        self.reader
            .reload()
            .map_err(|e| RagError::Index(format!("tantivy reload: {}", e)))?;
        Ok(())
    }

    /// BM25 top-`limit` parsed query, returning stored rows.
    pub fn search(&self, query: &str, limit: usize) -> RagResult<Vec<(LexRow, f32)>> {
        let query = Self::sanitize_query(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let parser = QueryParser::for_index(&self.index, vec![self.f_text]);
        let parsed = parser
            .parse_query(&query)
            .map_err(|e| RagError::Index(format!("tantivy parse query: {}", e)))?;
        let searcher = self.reader.searcher();
        let hits = searcher
            .search(&parsed, &TopDocs::with_limit(limit.max(1)).order_by_score())
            .map_err(|e| RagError::Index(format!("tantivy search: {}", e)))?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let doc = searcher
                .doc::<TantivyDocument>(addr)
                .map_err(|e| RagError::Index(format!("tantivy doc: {}", e)))?;
            let text = doc
                .get_first(self.f_text)
                .and_then(|v| Value::as_str(&v))
                .unwrap_or_default()
                .to_string();
            let path = doc
                .get_first(self.f_path)
                .and_then(|v| Value::as_str(&v))
                .unwrap_or("")
                .to_string();
            let lang = doc
                .get_first(self.f_lang)
                .and_then(|v| Value::as_str(&v))
                .unwrap_or("")
                .to_string();
            let doc_id = doc
                .get_first(self.f_doc_id)
                .and_then(|v| Value::as_u64(&v))
                .unwrap_or(u64::MAX);
            out.push((
                LexRow {
                    doc_id,
                    text,
                    path,
                    lang,
                },
                score,
            ));
        }
        Ok(out)
    }

    /// Stored row text for one doc id (回表 server for dense-lane hits);
    /// `None` when the lexical row was never written or was physically deleted.
    pub fn row_for(&self, doc_id: u64) -> RagResult<Option<String>> {
        let searcher = self.reader.searcher();
        let term = Term::from_field_u64(self.f_doc_id, doc_id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let hits = searcher
            .search(&query, &TopDocs::with_limit(1).order_by_score())
            .map_err(|e| RagError::Index(format!("tantivy row_for: {}", e)))?;
        if hits.is_empty() {
            return Ok(None);
        }
        let doc = searcher
            .doc::<TantivyDocument>(hits[0].1)
            .map_err(|e| RagError::Index(format!("tantivy doc: {}", e)))?;
        Ok(doc
            .get_first(self.f_text)
            .and_then(|v| Value::as_str(&v))
            .map(|s| s.to_string()))
    }

    /// Count indexed documents (commit-dependent approximation good enough
    /// for status display; the authoritative count is the mapping size).
    pub fn count(&self) -> RagResult<usize> {
        let searcher = self.reader.searcher();
        Ok(searcher.num_docs() as usize)
    }

    /// Escape Tantivy query grammar characters so raw identifier / text
    /// searches never fail to parse.
    fn sanitize_query(query: &str) -> String {
        let cleaned: String = query
            .chars()
            .map(|c| {
                if ":()[]{}+^\"~*?!\\/-".contains(c) {
                    ' '
                } else {
                    c
                }
            })
            .filter(|c| !c.is_whitespace() || *c == ' ')
            .collect();
        cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rag-tantivy-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_search_commit() {
        let dir = temp_dir("search");
        let mut lex = LexicalIndex::open(&dir).unwrap();
        lex.add_row(1, "fn parse_context_window(cfg)", "a.rs", "rust")
            .unwrap();
        lex.add_row(2, "docs about retrieval", "a.md", "md")
            .unwrap();
        lex.commit().unwrap();
        let hits = lex.search("parse_context_window", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0.doc_id, 1);
        assert!(hits[0].0.text.contains("parse_context_window"));
        assert!(hits[0].1 > 0.0, "BM25 score must be surfaced");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_physically_removes() {
        let dir = temp_dir("delete");
        let mut lex = LexicalIndex::open(&dir).unwrap();
        lex.add_row(1, "fn alpha()", "a.rs", "rust").unwrap();
        lex.commit().unwrap();
        lex.delete_rows(&[1]).unwrap();
        let hits = lex.search("alpha", 10).unwrap();
        assert!(hits.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
