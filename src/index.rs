#![allow(clippy::new_ret_no_self)]

use pyo3::{exceptions, prelude::*, types::PyAny};

use crate::{
    document::{extract_value, Document},
    get_field,
    query::Query,
    schema::Schema,
    searcher::Searcher,
    to_pyerr,
};
use tantivy as tv;
use tantivy::{
    directory::MmapDirectory,
    schema::{NamedFieldDocument, Term, Value},
    tokenizer::{
        Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
        TextAnalyzer,
    },
};

const RELOAD_POLICY: &str = "commit";

/// IndexWriter is the user entry-point to add documents to the index.
///
/// To create an IndexWriter first create an Index and call the writer() method
/// on the index object.
#[pyclass]
pub(crate) struct IndexWriter {
    inner_index_writer: tv::IndexWriter,
    schema: tv::schema::Schema,
}

#[pymethods]
impl IndexWriter {
    /// Add a document to the index.
    ///
    /// If the indexing pipeline is full, this call may block.
    ///
    /// Returns an `opstamp`, which is an increasing integer that can be used
    /// by the client to align commits with its own document queue.
    /// The `opstamp` represents the number of documents that have been added
    /// since the creation of the index.
    pub fn add_document(&mut self, doc: &Document) -> PyResult<u64> {
        let named_doc = NamedFieldDocument(doc.field_values.clone());
        let doc = self.schema.convert_named_doc(named_doc).map_err(to_pyerr)?;
        self.inner_index_writer.add_document(doc).map_err(to_pyerr)
    }

    /// Helper for the `add_document` method, but passing a json string.
    ///
    /// If the indexing pipeline is full, this call may block.
    ///
    /// Returns an `opstamp`, which is an increasing integer that can be used
    /// by the client to align commits with its own document queue.
    /// The `opstamp` represents the number of documents that have been added
    /// since the creation of the index.
    pub fn add_json(&mut self, json: &str) -> PyResult<u64> {
        let doc = self.schema.parse_document(json).map_err(to_pyerr)?;
        let opstamp = self.inner_index_writer.add_document(doc);
        opstamp.map_err(to_pyerr)
    }

    /// Commits all of the pending changes
    ///
    /// A call to commit blocks. After it returns, all of the document that
    /// were added since the last commit are published and persisted.
    ///
    /// In case of a crash or an hardware failure (as long as the hard disk is
    /// spared), it will be possible to resume indexing from this point.
    ///
    /// Returns the `opstamp` of the last document that made it in the commit.
    fn commit(&mut self) -> PyResult<u64> {
        self.inner_index_writer.commit().map_err(to_pyerr)
    }

    /// Rollback to the last commit
    ///
    /// This cancels all of the update that happened before after the last
    /// commit. After calling rollback, the index is in the same state as it
    /// was after the last commit.
    fn rollback(&mut self) -> PyResult<u64> {
        self.inner_index_writer.rollback().map_err(to_pyerr)
    }

    /// Detect and removes the files that are not used by the index anymore.
    fn garbage_collect_files(&mut self) -> PyResult<()> {
        use futures::executor::block_on;
        block_on(self.inner_index_writer.garbage_collect_files())
            .map_err(to_pyerr)?;
        Ok(())
    }

    /// The opstamp of the last successful commit.
    ///
    /// This is the opstamp the index will rollback to if there is a failure
    /// like a power surge.
    ///
    /// This is also the opstamp of the commit that is currently available
    /// for searchers.
    #[getter]
    fn commit_opstamp(&self) -> u64 {
        self.inner_index_writer.commit_opstamp()
    }

    /// Delete all documents containing a given term.
    ///
    /// Args:
    ///     field_name (str): The field name for which we want to filter deleted docs.
    ///     field_value (PyAny): Python object with the value we want to filter.
    ///
    /// If the field_name is not on the schema raises ValueError exception.
    /// If the field_value is not supported raises Exception.
    fn delete_documents(
        &mut self,
        field_name: &str,
        field_value: &PyAny,
    ) -> PyResult<u64> {
        let field = get_field(&self.schema, field_name)?;
        let value = extract_value(field_value)?;
        let term = match value {
            Value::Str(text) => Term::from_field_text(field, &text),
            Value::U64(num) => Term::from_field_u64(field, num),
            Value::I64(num) => Term::from_field_i64(field, num),
            Value::F64(num) => Term::from_field_f64(field, num),
            Value::Date(d) => Term::from_field_date(field, &d),
            Value::Facet(facet) => Term::from_facet(field, &facet),
            Value::Bytes(_) => {
                return Err(exceptions::PyValueError::new_err(format!(
                    "Field `{field_name}` is bytes type not deletable."
                )))
            }
            Value::PreTokStr(_pretok) => {
                return Err(exceptions::PyValueError::new_err(format!(
                    "Field `{field_name}` is pretokenized. This is not authorized for delete."
                )))
            }
            Value::JsonObject(_) => {
                return Err(exceptions::PyValueError::new_err(format!(
                    "Field `{field_name}` is json object type not deletable."
                )))
            },

        };
        Ok(self.inner_index_writer.delete_term(term))
    }
}

/// Create a new index object.
///
/// Args:
///     schema (Schema): The schema of the index.
///     path (str, optional): The path where the index should be stored. If
///         no path is provided, the index will be stored in memory.
///     reuse (bool, optional): Should we open an existing index if one exists
///         or always create a new one.
///
/// If an index already exists it will be opened and reused. Raises OSError
/// if there was a problem during the opening or creation of the index.
#[pyclass]
pub(crate) struct Index {
    pub(crate) index: tv::Index,
    reader: tv::IndexReader,
}

#[pymethods]
impl Index {
    #[staticmethod]
    fn open(path: &str) -> PyResult<Index> {
        let index = tv::Index::open_in_dir(path).map_err(to_pyerr)?;

        Index::register_custom_text_analyzers(&index);

        let reader = index.reader().map_err(to_pyerr)?;
        Ok(Index { index, reader })
    }

    #[new]
    #[pyo3(signature = (schema, path = None, reuse = true))]
    fn new(schema: &Schema, path: Option<&str>, reuse: bool) -> PyResult<Self> {
        let index = match path {
            Some(p) => {
                let directory = MmapDirectory::open(p).map_err(to_pyerr)?;
                if reuse {
                    tv::Index::open_or_create(directory, schema.inner.clone())
                } else {
                    tv::Index::create(
                        directory,
                        schema.inner.clone(),
                        tv::IndexSettings::default(),
                    )
                }
                .map_err(to_pyerr)?
            }
            None => tv::Index::create_in_ram(schema.inner.clone()),
        };

        Index::register_custom_text_analyzers(&index);

        let reader = index.reader().map_err(to_pyerr)?;
        Ok(Index { index, reader })
    }

    /// Create a `IndexWriter` for the index.
    ///
    /// The writer will be multithreaded and the provided heap size will be
    /// split between the given number of threads.
    ///
    /// Args:
    ///     overall_heap_size (int, optional): The total target memory usage of
    ///         the writer, can't be less than 3000000.
    ///     num_threads (int, optional): The number of threads that the writer
    ///         should use. If this value is 0, tantivy will choose
    ///         automatically the number of threads.
    ///
    /// Raises ValueError if there was an error while creating the writer.
    #[pyo3(signature = (heap_size = 3000000, num_threads = 0))]
    fn writer(
        &self,
        heap_size: usize,
        num_threads: usize,
    ) -> PyResult<IndexWriter> {
        let writer = match num_threads {
            0 => self.index.writer(heap_size),
            _ => self.index.writer_with_num_threads(num_threads, heap_size),
        }
        .map_err(to_pyerr)?;
        let schema = self.index.schema();
        Ok(IndexWriter {
            inner_index_writer: writer,
            schema,
        })
    }

    /// Configure the index reader.
    ///
    /// Args:
    ///     reload_policy (str, optional): The reload policy that the
    ///         IndexReader should use. Can be `Manual` or `OnCommit`.
    ///     num_warmers (int, optional): The number of searchers that the
    ///         reader should create.
    #[pyo3(signature = (reload_policy = RELOAD_POLICY, num_warmers = 0))]
    fn config_reader(
        &mut self,
        reload_policy: &str,
        num_warmers: usize,
    ) -> Result<(), PyErr> {
        let reload_policy = reload_policy.to_lowercase();
        let reload_policy = match reload_policy.as_ref() {
            "commit" => tv::ReloadPolicy::OnCommit,
            "on-commit" => tv::ReloadPolicy::OnCommit,
            "oncommit" => tv::ReloadPolicy::OnCommit,
            "manual" => tv::ReloadPolicy::Manual,
            _ => return Err(exceptions::PyValueError::new_err(
                "Invalid reload policy, valid choices are: 'manual' and 'OnCommit'"
            ))
        };
        let builder = self.index.reader_builder();
        let builder = builder.reload_policy(reload_policy);
        let builder = if num_warmers > 0 {
            builder.num_warming_threads(num_warmers)
        } else {
            builder
        };

        self.reader = builder.try_into().map_err(to_pyerr)?;
        Ok(())
    }

    /// Acquires a Searcher from the searcher pool.
    ///
    /// If no searcher is available during the call, note that
    /// this call will block until one is made available.
    ///
    /// Searcher are automatically released back into the pool when
    /// they are dropped. If you observe this function to block forever
    /// you probably should configure the Index to have a larger
    /// searcher pool, or you are holding references to previous searcher
    /// for ever.
    fn searcher(&self, py: Python) -> Searcher {
        Searcher {
            inner: py.allow_threads(|| self.reader.searcher())
            }
    }


    /// Check if the given path contains an existing index.
    /// Args:
    ///     path: The path where tantivy will search for an index.
    ///
    /// Returns True if an index exists at the given path, False otherwise.
    ///
    /// Raises OSError if the directory cannot be opened.
    #[staticmethod]
    fn exists(path: &str) -> PyResult<bool> {
        let directory = MmapDirectory::open(path).map_err(to_pyerr)?;
        tv::Index::exists(&directory).map_err(to_pyerr)
    }

    /// The schema of the current index.
    #[getter]
    fn schema(&self) -> Schema {
        let schema = self.index.schema();
        Schema { inner: schema }
    }

    /// Update searchers so that they reflect the state of the last .commit().
    ///
    /// If you set up the the reload policy to be on 'commit' (which is the
    /// default) every commit should be rapidly reflected on your IndexReader
    /// and you should not need to call reload() at all.
    fn reload(&self) -> PyResult<()> {
        self.reader.reload().map_err(to_pyerr)
    }

    /// Parse a query
    ///
    /// Args:
    ///     query: the query, following the tantivy query language.
    ///     default_fields_names (List[Field]): A list of fields used to search if no
    ///         field is specified in the query.
    ///
    #[pyo3(signature = (query, default_field_names = None))]
    pub fn parse_query(
        &self,
        query: &str,
        default_field_names: Option<Vec<String>>,
    ) -> PyResult<Query> {
        let mut default_fields = vec![];
        let schema = self.index.schema();
        if let Some(default_field_names_vec) = default_field_names {
            for default_field_name in &default_field_names_vec {
                if let Some(field) = schema.get_field(default_field_name) {
                    let field_entry = schema.get_field_entry(field);
                    if !field_entry.is_indexed() {
                        return Err(exceptions::PyValueError::new_err(
                            format!(
                            "Field `{default_field_name}` is not set as indexed in the schema."
                        ),
                        ));
                    }
                    default_fields.push(field);
                } else {
                    return Err(exceptions::PyValueError::new_err(format!(
                        "Field `{default_field_name}` is not defined in the schema."
                    )));
                }
            }
        } else {
            for (field, field_entry) in self.index.schema().fields() {
                if field_entry.is_indexed() {
                    default_fields.push(field);
                }
            }
        }
        let parser =
            tv::query::QueryParser::for_index(&self.index, default_fields);
        let query = parser.parse_query(query).map_err(to_pyerr)?;

        Ok(Query { inner: query })
    }
}

impl Index {
    fn register_custom_text_analyzers(index: &tv::Index) {
        let analyzers = [
            ("ar_stem", Language::Arabic),
            ("da_stem", Language::Danish),
            ("nl_stem", Language::Dutch),
            ("fi_stem", Language::Finnish),
            ("fr_stem", Language::French),
            ("de_stem", Language::German),
            ("el_stem", Language::Greek),
            ("hu_stem", Language::Hungarian),
            ("it_stem", Language::Italian),
            ("no_stem", Language::Norwegian),
            ("pt_stem", Language::Portuguese),
            ("ro_stem", Language::Romanian),
            ("ru_stem", Language::Russian),
            ("es_stem", Language::Spanish),
            ("sv_stem", Language::Swedish),
            ("ta_stem", Language::Tamil),
            ("tr_stem", Language::Turkish),
        ];

        for (name, lang) in &analyzers {
            let an = TextAnalyzer::from(SimpleTokenizer)
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                .filter(Stemmer::new(*lang));
            index.tokenizers().register(name, an);
        }
    }
}
