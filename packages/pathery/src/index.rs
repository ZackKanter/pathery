use crate::{
    directory::PatheryDirectory,
    schema::{SchemaLoader, SchemaProvider},
};
use std::{fs, path::Path, rc::Rc};
use tantivy::{schema::Field, Index, IndexWriter};

pub trait IndexLoader {
    fn load_index(&self, index_id: &str) -> Rc<Index>;
}

pub struct IndexProvider {
    schema_loader: SchemaProvider,
}

impl IndexProvider {
    pub fn lambda() -> Self {
        Self {
            schema_loader: SchemaProvider::lambda().expect("SchemaLoader should create"),
        }
    }
}

impl IndexLoader for IndexProvider {
    fn load_index(&self, index_id: &str) -> Rc<Index> {
        let directory_path = format!("/mnt/pathery-data/{index_id}");

        let index = if let Ok(existing_dir) = PatheryDirectory::open(&directory_path) {
            Index::open(existing_dir).expect("Index should be openable")
        } else {
            fs::create_dir(&directory_path).expect("Directory should be creatable");
            let schema = self.schema_loader.load_schema(index_id);
            Index::create_in_dir(Path::new(&directory_path), schema)
                .expect("Index should be creatable")
        };

        Rc::new(index)
    }
}

/// Used for testing purposes. Always returns the same Rc wrapped index.
impl IndexLoader for Rc<Index> {
    fn load_index(&self, _index_id: &str) -> Rc<Index> {
        Rc::clone(self)
    }
}

pub trait TantivyIndex {
    fn default_writer(&self) -> IndexWriter;

    fn id_field(&self) -> Field;
}

impl TantivyIndex for Index {
    fn default_writer(&self) -> IndexWriter {
        self.writer(100_000_000)
            .expect("Writer should be available")
    }

    fn id_field(&self) -> Field {
        self.schema()
            .get_field("__id")
            .expect("__id field should exist")
    }
}