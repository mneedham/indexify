use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fmt,
    hash::{Hash, Hasher},
    path::Path,
    sync::Arc,
    time::SystemTime,
};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use indexify_internal_api as internal_api;
use indexify_proto::indexify_coordinator;
use internal_api::ExtractedEmbeddings;
use itertools::Itertools;
use nanoid::nanoid;
use tracing::{error, info};

pub(crate) use crate::unwrap_or_continue;
use crate::{
    api::{self, BeginExtractedContentIngest},
    blob_storage::{BlobStorage, BlobStorageWriter},
    coordinator_client::CoordinatorClient,
    grpc_helper::GrpcHelper,
    metadata_storage::{ExtractedMetadata, MetadataStorageTS},
    utils::OptionInspectNone,
    vector_index::{ScoredText, VectorIndexManager},
};

pub struct DataManager {
    vector_index_manager: Arc<VectorIndexManager>,
    metadata_index_manager: MetadataStorageTS,
    blob_storage: Arc<BlobStorage>,
    coordinator_client: Arc<CoordinatorClient>,
}

impl fmt::Debug for DataManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataManager").finish()
    }
}

impl DataManager {
    pub async fn new(
        vector_index_manager: Arc<VectorIndexManager>,
        metadata_index_manager: MetadataStorageTS,
        blob_storage: Arc<BlobStorage>,
        coordinator_client: Arc<CoordinatorClient>,
    ) -> Result<Self> {
        Ok(Self {
            vector_index_manager,
            metadata_index_manager,
            blob_storage,
            coordinator_client,
        })
    }

    #[tracing::instrument]
    pub async fn list_namespaces(&self) -> Result<Vec<api::DataNamespace>> {
        let req = indexify_coordinator::ListNamespaceRequest {};
        let response = self.coordinator_client.get().await?.list_ns(req).await?;
        let namespaces = response.into_inner().namespaces;
        let data_namespaces = namespaces
            .into_iter()
            .map(|r| api::DataNamespace {
                name: r.name,
                extraction_policies: Vec::new(),
            })
            .collect();
        Ok(data_namespaces)
    }

    #[tracing::instrument]
    pub async fn create(&self, namespace: &api::DataNamespace) -> Result<()> {
        info!("creating data namespace: {}", namespace.name);
        let policies = namespace
            .extraction_policies
            .clone()
            .into_iter()
            .map(|b| b.into())
            .collect();
        let _ = self
            .metadata_index_manager
            .create_metadata_table(&namespace.name)
            .await?;
        let request = indexify_coordinator::CreateNamespaceRequest {
            name: namespace.name.clone(),
            policies,
        };
        let _resp = self
            .coordinator_client
            .get()
            .await?
            .create_ns(request)
            .await?;
        Ok(())
    }

    #[tracing::instrument]
    pub async fn get(&self, name: &str) -> Result<api::DataNamespace> {
        let req = indexify_coordinator::GetNamespaceRequest {
            name: name.to_string(),
        };
        let response = self
            .coordinator_client
            .get()
            .await?
            .get_ns(req)
            .await?
            .into_inner();
        let namespace = response.namespace.ok_or(anyhow!("namespace not found"))?;
        namespace.try_into()
    }

    pub async fn create_extraction_policy(
        &self,
        namespace: &str,
        extraction_policy: &api::ExtractionPolicy,
    ) -> Result<Vec<String>> {
        info!(
            "adding extractor bindings namespace: {}, extractor: {}, binding: {}",
            namespace, extraction_policy.extractor, extraction_policy.name,
        );
        let req = indexify_coordinator::ExtractionPolicyRequest {
            namespace: namespace.to_string(),
            policy: Some(extraction_policy.clone().into()),
            created_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)?
                .as_secs() as i64,
        };
        let response = self
            .coordinator_client
            .get()
            .await?
            .create_extraction_policy(req)
            .await?
            .into_inner();
        let mut index_names = Vec::new();
        let extractor = response.extractor.ok_or(anyhow!(
            "extractor {:?} not found",
            extraction_policy.extractor
        ))?;
        for (name, output_schema) in &extractor.outputs {
            let output_schema: internal_api::OutputSchema = serde_json::from_str(output_schema)?;
            let index_name = response.output_index_name_mapping.get(name).unwrap();
            let table_name = response.index_name_table_mapping.get(index_name).unwrap();
            index_names.push(index_name.clone());
            let schema_json = serde_json::to_value(&output_schema)?;
            let _ = match output_schema {
                internal_api::OutputSchema::Embedding(embedding_schema) => {
                    let _ = self
                        .vector_index_manager
                        .create_index(table_name, embedding_schema.clone())
                        .await?;
                }
                _ => {}
            };
            self.create_index_metadata(
                namespace,
                index_name,
                table_name,
                schema_json,
                &extraction_policy.name,
                &extractor.name,
            )
            .await?;
        }

        Ok(index_names)
    }

    async fn create_index_metadata(
        &self,
        namespace: &str,
        index_name: &str,
        table_name: &str,
        schema: serde_json::Value,
        extraction_policy: &str,
        extractor: &str,
    ) -> Result<()> {
        let index = indexify_coordinator::CreateIndexRequest {
            index: Some(indexify_coordinator::Index {
                name: index_name.to_string(),
                table_name: table_name.to_string(),
                namespace: namespace.to_string(),
                schema: serde_json::to_value(schema).unwrap().to_string(),
                extraction_policy: extraction_policy.to_string(),
                extractor: extractor.to_string(),
            }),
        };
        let req = GrpcHelper::into_req(index);
        let _resp = self
            .coordinator_client
            .get()
            .await?
            .create_index(req)
            .await?;
        Ok(())
    }

    pub async fn list_content(
        &self,
        namespace: &str,
        source_filter: &str,
        parent_id_filter: &str,
        labels_eq_filter: Option<&HashMap<String, String>>,
    ) -> Result<Vec<api::ContentMetadata>> {
        let req = indexify_coordinator::ListContentRequest {
            namespace: namespace.to_string(),
            source: source_filter.to_string(),
            parent_id: parent_id_filter.to_string(),
            labels_eq: labels_eq_filter.unwrap_or(&HashMap::new()).clone(),
        };
        let response = self
            .coordinator_client
            .get()
            .await?
            .list_content(req)
            .await?;
        let content_list = response
            .into_inner()
            .content_list
            .into_iter()
            .map(|c| c.into())
            .collect_vec();
        Ok(content_list)
    }

    #[tracing::instrument]
    pub async fn add_texts(&self, namespace: &str, content_list: Vec<api::Content>) -> Result<()> {
        for text in content_list {
            let size_bytes = text.bytes.len() as u64;
            let content_metadata = self
                .write_content(namespace, text, None, None, "ingestion", size_bytes)
                .await?;
            let req: indexify_coordinator::CreateContentRequest =
                indexify_coordinator::CreateContentRequest {
                    content: Some(content_metadata),
                };
            self.coordinator_client
                .get()
                .await?
                .create_content(GrpcHelper::into_req(req))
                .await
                .map_err(|e| {
                    anyhow!(
                        "unable to write content metadata to coordinator {}",
                        e.to_string()
                    )
                })?;
        }
        Ok(())
    }

    pub async fn get_content_metadata(
        &self,
        _namespace: &str,
        content_ids: Vec<String>,
    ) -> Result<Vec<api::ContentMetadata>> {
        let req = indexify_coordinator::GetContentMetadataRequest {
            content_list: content_ids,
        };
        let response = self
            .coordinator_client
            .get()
            .await?
            .get_content_metadata(req)
            .await?;
        let content_metadata_list = response.into_inner().content_list;
        let mut content_list = Vec::new();
        for c in content_metadata_list {
            content_list.push(c.into())
        }
        Ok(content_list)
    }

    #[tracing::instrument(skip(self, data))]
    pub async fn upload_file(&self, namespace: &str, data: Bytes, name: &str) -> Result<()> {
        let ext = Path::new(name)
            .extension()
            .unwrap_or_default()
            .to_str()
            .unwrap_or_default();
        let content_mime = mime_guess::from_ext(ext).first_or_octet_stream();
        let content = api::Content {
            content_type: content_mime.to_string(),
            bytes: data.to_vec(),
            labels: HashMap::new(),
            features: vec![],
        };
        let size_bytes = data.len() as u64;
        let content_metadata = self
            .write_content(
                namespace,
                content,
                Some(name),
                None,
                "ingestion",
                size_bytes,
            )
            .await
            .map_err(|e| anyhow!("unable to write content to blob store: {}", e))?;
        let req = indexify_coordinator::CreateContentRequest {
            content: Some(content_metadata),
        };
        self.coordinator_client
            .get()
            .await?
            .create_content(GrpcHelper::into_req(req))
            .await
            .map_err(|e| {
                anyhow!(
                    "unable to write content metadata to coordinator {}",
                    e.to_string()
                )
            })?;
        Ok(())
    }

    async fn write_content(
        &self,
        namespace: &str,
        content: api::Content,
        file_name: Option<&str>,
        parent_id: Option<String>,
        source: &str,
        size_bytes: u64,
    ) -> Result<indexify_coordinator::ContentMetadata> {
        let current_ts_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let mut s = DefaultHasher::new();
        namespace.hash(&mut s);
        let file_name = file_name.map(|f| f.to_string()).unwrap_or(nanoid!());
        file_name.hash(&mut s);
        if let Some(parent_id) = &parent_id {
            parent_id.hash(&mut s);
        }
        let id = format!("{:x}", s.finish());
        let storage_url = self
            .write_to_blob_store(namespace, &file_name, Bytes::from(content.bytes.clone()))
            .await
            .map_err(|e| anyhow!("unable to write text to blob store: {}", e))?;
        let labels = content
            .labels
            .clone()
            .into_iter()
            .map(|(k, v)| (k, v.to_string()))
            .collect();
        Ok(indexify_coordinator::ContentMetadata {
            id,
            file_name,
            storage_url,
            parent_id: parent_id.unwrap_or_default(),
            created_at: current_ts_secs as i64,
            mime: content.content_type,
            namespace: namespace.to_string(),
            labels,
            source: source.to_string(),
            size_bytes,
        })
    }

    pub async fn finish_extracted_content_write(
        &self,
        _begin_ingest: BeginExtractedContentIngest,
    ) -> Result<()> {
        let outcome: indexify_coordinator::TaskOutcome = _begin_ingest.task_outcome.into();

        let req = indexify_coordinator::UpdateTaskRequest {
            executor_id: _begin_ingest.executor_id,
            task_id: _begin_ingest.task_id,
            outcome: outcome as i32,
            content_list: Vec::new(),
        };
        let res = self.coordinator_client.get().await?.update_task(req).await;
        if let Err(err) = res {
            error!("unable to update task: {}", err.to_string());
        }
        Ok(())
    }

    pub async fn write_extracted_content(
        &self,
        ingest_metadata: BeginExtractedContentIngest,
        extracted_content: api::ExtractedContent,
    ) -> Result<()> {
        let namespace = ingest_metadata.namespace.clone();
        let mut new_content_metadata = Vec::new();
        for content in extracted_content.content_list {
            let content: api::Content = content.into();
            let size_bytes = content.bytes.len() as u64;
            let content_metadata = self
                .write_content(
                    namespace.as_str(),
                    content.clone(),
                    None,
                    Some(ingest_metadata.parent_content_id.to_string()),
                    &ingest_metadata.extraction_policy,
                    size_bytes,
                )
                .await?;
            new_content_metadata.push(content_metadata.clone());
            let mut new_embeddings: HashMap<&str, Vec<ExtractedEmbeddings>> = HashMap::new();
            for feature in content.features {
                let index_table_name = ingest_metadata
                    .output_to_index_table_mapping
                    .get(&feature.name);
                let index_table_name = unwrap_or_continue!(index_table_name.inspect_none(|| {
                    error!(
                        "unable to find index table name for feature {}",
                        feature.name
                    )
                }));
                match feature.feature_type {
                    api::FeatureType::Embedding => {
                        let embedding_payload: internal_api::Embedding =
                            serde_json::from_value(feature.data).map_err(|e| {
                                anyhow!("unable to get embedding from extracted data {}", e)
                            })?;
                        let embeddings = internal_api::ExtractedEmbeddings {
                            content_id: content_metadata.id.to_string(),
                            embedding: embedding_payload.values,
                        };
                        new_embeddings
                            .entry(index_table_name)
                            .or_default()
                            .push(embeddings);
                    }
                    api::FeatureType::Metadata => {
                        let extracted_attributes = ExtractedMetadata::new(
                            &content_metadata.id,
                            &content_metadata.parent_id,
                            feature.data.clone(),
                            "extractor_name",
                            &ingest_metadata.extraction_policy,
                            &namespace,
                        );
                        info!("adding metadata to index {}", feature.data.to_string());
                        self.metadata_index_manager
                            .add_metadata(&namespace, extracted_attributes)
                            .await?;
                    }
                    _ => {}
                }
            }
            for (index_table_name, embeddings) in new_embeddings {
                self.vector_index_manager
                    .add_embedding(index_table_name, embeddings)
                    .await
                    .map_err(|e| anyhow!("unable to add embedding to vector index {}", e))?;
            }
        }
        for content_meta in new_content_metadata {
            let req = indexify_coordinator::CreateContentRequest {
                content: Some(content_meta),
            };
            self.coordinator_client
                .get()
                .await?
                .create_content(GrpcHelper::into_req(req))
                .await
                .map_err(|e| {
                    anyhow!(
                        "unable to write content metadata to coordinator {}",
                        e.to_string()
                    )
                })?;
        }
        Ok(())
    }

    #[tracing::instrument]
    pub async fn list_indexes(&self, namespace: &str) -> Result<Vec<api::Index>> {
        let req = indexify_coordinator::ListIndexesRequest {
            namespace: namespace.to_string(),
        };
        let resp = self
            .coordinator_client
            .get()
            .await?
            .list_indexes(req)
            .await?;
        let mut api_indexes = Vec::new();
        for index in resp.into_inner().indexes {
            let schema: api::ExtractorOutputSchema =
                serde_json::from_str(&index.schema).map_err(|e| {
                    anyhow!(
                        "unable to parse schema for index {} {}",
                        index.name,
                        e.to_string()
                    )
                })?;
            api_indexes.push(api::Index {
                name: index.name,
                schema,
            });
        }
        Ok(api_indexes)
    }

    #[tracing::instrument]
    pub async fn search(
        &self,
        namespace: &str,
        index_name: &str,
        query: &str,
        k: u64,
    ) -> Result<Vec<ScoredText>> {
        let req = indexify_coordinator::GetIndexRequest {
            namespace: namespace.to_string(),
            name: index_name.to_string(),
        };
        let index = self
            .coordinator_client
            .get()
            .await?
            .get_index(req)
            .await?
            .into_inner()
            .index
            .ok_or(anyhow!("Index not found"))?;
        self.vector_index_manager
            .search(index, query, k as usize)
            .await
    }

    #[tracing::instrument]
    pub async fn metadata_lookup(
        &self,
        namespace: &str,
        content_id: &str,
    ) -> Result<Vec<ExtractedMetadata>, anyhow::Error> {
        self.metadata_index_manager
            .get_metadata(namespace, content_id)
            .await
    }

    #[tracing::instrument]
    pub async fn list_extractors(&self) -> Result<Vec<api::ExtractorDescription>> {
        let req = indexify_coordinator::ListExtractorsRequest {};
        let response = self
            .coordinator_client
            .get()
            .await?
            .list_extractors(req)
            .await?
            .into_inner();

        let extractors = response
            .extractors
            .into_iter()
            .map(|e| e.try_into())
            .collect::<Result<Vec<api::ExtractorDescription>>>()?;
        Ok(extractors)
    }

    #[tracing::instrument]
    async fn write_to_blob_store(
        &self,
        namespace: &str,
        name: &str,
        file: Bytes,
    ) -> Result<String> {
        self.blob_storage.put(name, file).await
    }
}
