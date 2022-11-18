use pathery::index::{IndexLoader, IndexProvider, TantivyIndex};
use pathery::lambda::lambda_runtime::{run, service_fn};
use pathery::lambda::sqs;
use pathery::lambda::{self, tracing};
use pathery::message::{WriterMessage, WriterMessageDetail};
use pathery::tantivy::{Document, IndexWriter, Term};
use pathery::{json, tokio};
use std::collections::HashMap;

pub fn delete_doc(writer: &IndexWriter, doc_id: &str) {
    let index = writer.index();
    let id_field = index.id_field();

    writer.delete_term(Term::from_field_text(id_field, doc_id));
    tracing::info!(message = "doc_deleted", doc_id);
}

pub fn index_doc(writer: &IndexWriter, doc: Document) {
    let index = writer.index();
    let id_field = index.id_field();
    let doc_id = doc
        .get_first(id_field)
        .and_then(|id| id.as_text())
        .expect("__id field should be present")
        .to_string();

    delete_doc(writer, &doc_id);
    writer
        .add_document(doc)
        .expect("Adding a document should not error");
    tracing::info!(message = "doc_indexed", doc_id);
}

#[tokio::main]
async fn main() -> Result<(), sqs::Error> {
    lambda::init_tracing();

    let index_loader = &IndexProvider::lambda();

    let handler = |event: sqs::SqsEvent| async move {
        let records = event.payload.records;

        let messages = records
            .iter()
            .map(|message| message.body.as_ref().expect("Body should be present"))
            .map(|body| {
                let msg = json::from_str::<WriterMessage>(body.as_str())
                    .expect("Message should be deserializable");
                msg
            })
            .collect::<Vec<_>>();

        let mut writers: HashMap<String, IndexWriter> = HashMap::new();

        for message in messages {
            let index_id = message.index_id;
            let writer = writers
                .entry(index_id.to_string())
                .or_insert_with(|| index_loader.load_index(&index_id).default_writer());
            match message.detail {
                WriterMessageDetail::IndexSingleDoc { document } => index_doc(writer, document),
                WriterMessageDetail::DeleteSingleDoc { doc_id } => delete_doc(writer, &doc_id),
            }
        }

        for (index_id, writer) in writers.into_iter() {
            let mut writer = writer;
            writer.commit().expect("commit should succeed");
            tracing::info!(message = "index_committed", index_id);
            writer
                .wait_merging_threads()
                .expect("merge should finish without error");
        }

        Ok::<(), sqs::Error>(())
    };

    run(service_fn(handler)).await
}
