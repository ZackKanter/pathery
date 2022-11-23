use std::collections::HashMap;

use serde::{self, Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{DocParsingError, Field, Schema};
use tantivy::{DocAddress, Document, Score, SnippetGenerator, TantivyError};
use {serde_json as json, tracing};

use crate::index::IndexLoader;
use crate::lambda::http::{self, HandlerResult, ServiceRequest};
use crate::schema::{SchemaLoader, TantivySchema};
use crate::util;
use crate::worker::index_writer;
use crate::worker::index_writer::client::IndexWriterClient;

#[derive(Serialize, Deserialize, Debug)]
pub struct PathParams {
    index_id: String,
}

#[derive(Serialize)]
pub struct PostIndexResponse {
    #[serde(rename = "__id")]
    pub doc_id: String,
    pub updated_at: String,
}

enum IndexDocError {
    NotJsonObject,
    EmptyDoc,
    DocParsingError(DocParsingError),
}

impl From<IndexDocError> for HandlerResult {
    fn from(err: IndexDocError) -> Self {
        match err {
            IndexDocError::EmptyDoc => {
                return Ok(http::err_response(400, "Request JSON object is empty"))
            }
            IndexDocError::NotJsonObject => {
                return Ok(http::err_response(400, "Expected JSON object"))
            }
            IndexDocError::DocParsingError(err) => {
                return Ok(http::err_response(400, &err.to_string()));
            }
        }
    }
}

fn index_doc(json_doc: json::Value, schema: &Schema) -> Result<(String, Document), IndexDocError> {
    let json_doc = if let json::Value::Object(obj) = json_doc {
        obj
    } else {
        return Err(IndexDocError::NotJsonObject);
    };

    let doc_id = json_doc
        .get("__id")
        .and_then(|v| v.as_str())
        .map(|v| String::from(v));

    let mut document = schema
        .json_object_to_doc(json_doc)
        .map_err(|err| IndexDocError::DocParsingError(err))?;

    if document.is_empty() {
        return Err(IndexDocError::EmptyDoc);
    }

    match doc_id {
        Some(doc_id) => Ok((doc_id.into(), document)),
        None => {
            let id_field = schema.id_field();
            let doc_id = util::generate_id();
            document.add_text(id_field, &doc_id);
            Ok((doc_id, document))
        }
    }
}

// Indexes a document supplied via a JSON object in the body.
#[tracing::instrument(skip(writer_client, schema_loader))]
pub async fn post_index(
    writer_client: &IndexWriterClient,
    schema_loader: &dyn SchemaLoader,
    request: ServiceRequest<json::Value, PathParams>,
) -> HandlerResult {
    let (body, path_params) = match request.into_parts() {
        Ok(parts) => parts,
        Err(response) => return Ok(response),
    };

    let schema = schema_loader.load_schema(&path_params.index_id);

    let (doc_id, index_doc) = match index_doc(body, &schema) {
        Ok(doc) => doc,
        Err(err) => return err.into(),
    };

    let mut batch = index_writer::batch(&path_params.index_id);

    batch.index_doc(index_doc);

    writer_client.write_batch(batch).await;

    http::success(&PostIndexResponse {
        doc_id,
        updated_at: util::timestamp(),
    })
}

// Indexes a batch of documents
#[tracing::instrument(skip(writer_client, schema_loader))]
pub async fn batch_index(
    writer_client: &IndexWriterClient,
    schema_loader: &dyn SchemaLoader,
    request: ServiceRequest<Vec<json::Value>, PathParams>,
) -> HandlerResult {
    let (body, path_params) = match request.into_parts() {
        Ok(parts) => parts,
        Err(response) => return Ok(response),
    };

    let schema = schema_loader.load_schema(&path_params.index_id);

    let mut batch = index_writer::batch(&path_params.index_id);

    for doc_obj in body.into_iter() {
        let (_id, document) = match index_doc(doc_obj, &schema) {
            Ok(doc) => doc,
            Err(err) => return err.into(),
        };

        batch.index_doc(document);
    }

    writer_client.write_batch(batch).await;

    http::success(&PostIndexResponse {
        doc_id: "".into(),
        updated_at: util::timestamp(),
    })
}

#[derive(Serialize, Deserialize, Debug)]
pub struct QueryRequest {
    pub query: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SearchHit {
    pub doc: json::Value,
    pub snippets: json::Value,
    pub score: f32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct QueryResponse {
    pub matches: Vec<SearchHit>,
}

pub async fn query_index(
    index_loader: &dyn IndexLoader,
    request: ServiceRequest<QueryRequest, PathParams>,
) -> HandlerResult {
    let (body, path_params) = match request.into_parts() {
        Ok(parts) => parts,
        Err(response) => return Ok(response),
    };

    let index = index_loader.load_index(&path_params.index_id);

    let reader = index.reader().expect("Reader should load");

    let searcher = reader.searcher();

    let schema = index.schema();

    let query_parser = QueryParser::for_index(
        &index,
        schema
            .fields()
            .filter(|(_, config)| config.is_indexed())
            .map(|(field, _)| field)
            .collect::<Vec<Field>>(),
    );

    let query = query_parser.parse_query(&body.query)?;

    let top_docs: Vec<(Score, DocAddress)> = searcher.search(&query, &TopDocs::with_limit(10))?;

    let matches: Vec<_> = top_docs
        .into_iter()
        .map(|(score, address)| -> SearchHit {
            let document = searcher.doc(address).expect("doc should exist");

            let named_doc = schema.to_named_doc(&document);

            let snippets: HashMap<String, String> = document
                .field_values()
                .iter()
                .filter_map(|field_value| {
                    // Only text fields are supported for snippets
                    let text = field_value.value().as_text()?;

                    let generator =
                        match SnippetGenerator::create(&searcher, &query, field_value.field()) {
                            Ok(generator) => Some(generator),
                            // InvalidArgument is returned when field is not indexed
                            Err(TantivyError::InvalidArgument(_)) => None,
                            Err(err) => panic!("{}", err.to_string()),
                        }?;

                    let snippet = generator.snippet(text).to_html();

                    if snippet.is_empty() {
                        None
                    } else {
                        Some((schema.get_field_name(field_value.field()).into(), snippet))
                    }
                })
                .collect();

            SearchHit {
                score,
                doc: json::to_value(named_doc).expect("named doc should serialize"),
                snippets: json::to_value(snippets).expect("snippets should serialize"),
            }
        })
        .collect();

    http::success(&QueryResponse { matches })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::marker::PhantomData;
    use std::sync::Arc;
    use std::vec;

    use ::http::{Request, StatusCode};
    use async_trait::async_trait;
    use aws_lambda_events::query_map::QueryMap;
    use lambda_http::{Body, RequestExt};
    use serde::Deserialize;
    use tantivy::schema::{self, Schema};
    use tantivy::{doc, Index};

    use super::*;
    use crate::aws::{S3Bucket, S3Ref, SQSQueue};
    use crate::index::TantivyIndex;
    use crate::lambda::http::{HandlerResponse, HttpRequest};
    use crate::schema::SchemaProvider;

    fn test_index_writer_client() -> IndexWriterClient {
        struct TestBucketClient<O> {
            object_type: PhantomData<O>,
        }

        #[async_trait]
        impl<O: Send + Sync> S3Bucket<O> for TestBucketClient<O> {
            async fn write_object(&self, key: &str, _obj: &O) -> Option<S3Ref> {
                Some(S3Ref {
                    bucket: "test".into(),
                    key: key.into(),
                })
            }

            async fn read_object(&self, _s3_ref: &S3Ref) -> Option<O> {
                todo!()
            }

            async fn delete_object(&self, _s3_ref: &S3Ref) {
                todo!()
            }
        }

        struct TestQueueClient<O> {
            object_type: PhantomData<O>,
        }

        #[async_trait]
        impl<O: Send + Sync> SQSQueue<O> for TestQueueClient<O> {
            async fn send_message(&self, _group_id: &str, _message: &O) {}
        }

        IndexWriterClient {
            bucket_client: Box::new(TestBucketClient {
                object_type: PhantomData,
            }),
            queue_client: Box::new(TestQueueClient {
                object_type: PhantomData,
            }),
        }
    }

    fn setup() -> (IndexWriterClient, SchemaProvider) {
        let config = json::json!({
            "indexes": [
                {
                    "prefix": "test",
                    "fields": [
                        {
                            "name": "title",
                            "kind": "text",
                            "flags": ["TEXT"]
                        },
                        {
                            "name": "author",
                            "kind": "text",
                            "flags": ["TEXT"]
                        }
                    ]
                }
            ]
        });
        (
            test_index_writer_client(),
            SchemaProvider::from_json(config),
        )
    }

    fn request<B>(index_id: &str, body: B) -> ServiceRequest<B, PathParams>
    where B: Serialize {
        let request: HttpRequest = Request::builder()
            .header("Content-Type", "application/json")
            .body(json::to_string(&body).expect("should serialize").into())
            .expect("should build request");

        request
            .with_path_parameters::<QueryMap>(
                HashMap::from([(String::from("index_id"), String::from(index_id))]).into(),
            )
            .into()
    }

    fn parse_response<V>(response: HandlerResponse) -> (StatusCode, V)
    where V: for<'de> Deserialize<'de> {
        let code = response.status();
        let body: V = if let Body::Text(x) = response.body() {
            json::from_str(x).unwrap()
        } else {
            panic!("Invalid body")
        };
        (code, body)
    }

    #[tokio::test]
    async fn post_index_doc_with_no_id() {
        let (client, loader) = setup();

        let doc = json::json!({
            "title": "Zen and the Art of Motorcycle Maintenance",
            "author": "Robert Pirsig"
        });

        let request = request("test", doc);

        let response = post_index(&client, &loader, request).await.unwrap();

        let (code, _body) = parse_response::<json::Value>(response);

        assert_eq!(code, 200);
    }

    #[tokio::test]
    async fn post_index_non_object() {
        let (client, loader) = setup();

        let doc = json::json!([]);

        let request = request("test", doc);

        let response = post_index(&client, &loader, request).await.unwrap();

        let (code, body) = parse_response::<json::Value>(response);

        assert_eq!(code, 400);
        assert_eq!(body, json::json!({"message": "Expected JSON object"}));
    }

    #[tokio::test]
    async fn post_index_value_that_does_not_match_schema() {
        let (client, loader) = setup();

        let doc = json::json!({"title": 1});

        let request = request("test", doc);

        let response = post_index(&client, &loader, request).await.unwrap();

        let (code, body) = parse_response::<json::Value>(response);

        assert_eq!(code, 400);
        assert_eq!(
            body,
            json::json!({"message": "The field '\"title\"' could not be parsed: TypeError { expected: \"a string\", json: Number(1) }"})
        );
    }

    #[tokio::test]
    async fn post_index_field_that_does_not_exist() {
        let (client, loader) = setup();

        let doc = json::json!({
            "foobar": "baz",
        });

        let request = request("test", doc);

        let response = post_index(&client, &loader, request).await.unwrap();

        let (code, body) = parse_response::<json::Value>(response);

        assert_eq!(code, 400);
        // Empty because the non-existent field does not explicitly trigger a failure - it just
        // doesn't get indexed.
        assert_eq!(
            body,
            json::json!({"message": "Request JSON object is empty"})
        );
    }

    #[tokio::test]
    async fn query_default_response() {
        let mut schema = Schema::builder();
        let title = schema.add_text_field("title", schema::STORED | schema::TEXT);
        let author = schema.add_text_field("author", schema::STORED | schema::TEXT);
        let index = Index::create_in_ram(schema.build());
        let mut writer = index.default_writer();

        writer
            .add_document(doc!(
                title => "hello",
                author => "world",
            ))
            .unwrap();

        writer.commit().unwrap();

        let request = request(
            "test",
            QueryRequest {
                query: String::from("hello"),
            },
        );

        let response = query_index(&Arc::new(index), request).await.unwrap();

        let (status, body) = parse_response::<QueryResponse>(response);

        assert_eq!(200, status);
        assert_eq!(
            body,
            QueryResponse {
                matches: vec![SearchHit {
                    doc: json::json!({
                        "title": ["hello"],
                        "author": ["world"],
                    }),
                    score: 0.28768212,
                    snippets: json::json!({
                        "title": "<b>hello</b>"
                    })
                }]
            }
        );
    }

    #[tokio::test]
    async fn query_document_with_un_indexed_fields() {
        let mut schema = Schema::builder();
        let title = schema.add_text_field("title", schema::STORED | schema::STRING);
        let author = schema.add_text_field("author", schema::STORED);
        let index = Index::create_in_ram(schema.build());
        let mut writer = index.default_writer();

        writer
            .add_document(doc!(
                title => "hello",
                author => "world",
            ))
            .unwrap();

        writer.commit().unwrap();

        let request = request(
            "test",
            QueryRequest {
                query: String::from("hello"),
            },
        );

        let response = query_index(&Arc::new(index), request).await.unwrap();

        let (status, body) = parse_response::<QueryResponse>(response);

        assert_eq!(200, status);
        assert_eq!(1, body.matches.len());
    }
}
