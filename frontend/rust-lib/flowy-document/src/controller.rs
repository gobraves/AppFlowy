use crate::{
    context::DocumentUser,
    core::ClientDocumentEditor,
    errors::FlowyError,
    ws_receivers::DocumentWSReceivers,
    DocumentCloudService,
};
use bytes::Bytes;
use dashmap::DashMap;
use flowy_collaboration::entities::{
    doc::{DocumentDelta, DocumentId},
    revision::{md5, RepeatedRevision, Revision},
};
use flowy_database::ConnectionPool;
use flowy_error::FlowyResult;
use flowy_sync::{RevisionCache, RevisionCloudService, RevisionManager, RevisionWebSocket, WSStateReceiver};
use lib_infra::future::FutureResult;
use std::sync::Arc;

pub struct DocumentController {
    cloud_service: Arc<dyn DocumentCloudService>,
    ws_receivers: Arc<DocumentWSReceivers>,
    ws_sender: Arc<dyn RevisionWebSocket>,
    open_cache: Arc<OpenDocCache>,
    user: Arc<dyn DocumentUser>,
}

impl DocumentController {
    pub(crate) fn new(
        cloud_service: Arc<dyn DocumentCloudService>,
        user: Arc<dyn DocumentUser>,
        ws_receivers: Arc<DocumentWSReceivers>,
        ws_sender: Arc<dyn RevisionWebSocket>,
    ) -> Self {
        let open_cache = Arc::new(OpenDocCache::new());
        Self {
            cloud_service,
            ws_receivers,
            ws_sender,
            open_cache,
            user,
        }
    }

    pub(crate) fn init(&self) -> FlowyResult<()> {
        let notify = self.ws_sender.subscribe_state_changed();
        listen_ws_state_changed(notify, self.ws_receivers.clone());

        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, doc_id), fields(doc_id), err)]
    pub async fn open_document<T: AsRef<str>>(&self, doc_id: T) -> Result<Arc<ClientDocumentEditor>, FlowyError> {
        let doc_id = doc_id.as_ref();
        tracing::Span::current().record("doc_id", &doc_id);
        self.get_editor(doc_id).await
    }

    #[tracing::instrument(level = "debug", skip(self, doc_id), fields(doc_id), err)]
    pub fn close_document<T: AsRef<str>>(&self, doc_id: T) -> Result<(), FlowyError> {
        let doc_id = doc_id.as_ref();
        tracing::Span::current().record("doc_id", &doc_id);
        self.open_cache.remove(doc_id);
        self.ws_receivers.remove(doc_id);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, doc_id), fields(doc_id), err)]
    pub fn delete<T: AsRef<str>>(&self, doc_id: T) -> Result<(), FlowyError> {
        let doc_id = doc_id.as_ref();
        tracing::Span::current().record("doc_id", &doc_id);
        self.open_cache.remove(doc_id);
        self.ws_receivers.remove(doc_id);
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, delta), fields(doc_id = %delta.doc_id), err)]
    pub async fn receive_local_delta(&self, delta: DocumentDelta) -> Result<DocumentDelta, FlowyError> {
        let editor = self.get_editor(&delta.doc_id).await?;
        let _ = editor.compose_local_delta(Bytes::from(delta.delta_json)).await?;
        let document_json = editor.document_json().await?;
        Ok(DocumentDelta {
            doc_id: delta.doc_id.clone(),
            delta_json: document_json,
        })
    }

    pub async fn save_document<T: AsRef<str>>(&self, doc_id: T, revisions: RepeatedRevision) -> FlowyResult<()> {
        let doc_id = doc_id.as_ref().to_owned();
        let db_pool = self.user.db_pool()?;
        let rev_manager = self.make_rev_manager(&doc_id, db_pool)?;
        let _ = rev_manager.reset_object(revisions).await?;
        Ok(())
    }

    async fn get_editor(&self, doc_id: &str) -> FlowyResult<Arc<ClientDocumentEditor>> {
        match self.open_cache.get(doc_id) {
            None => {
                let db_pool = self.user.db_pool()?;
                self.make_editor(&doc_id, db_pool).await
            },
            Some(editor) => Ok(editor),
        }
    }
}

impl DocumentController {
    async fn make_editor(
        &self,
        doc_id: &str,
        pool: Arc<ConnectionPool>,
    ) -> Result<Arc<ClientDocumentEditor>, FlowyError> {
        let user = self.user.clone();
        let token = self.user.token()?;
        let rev_manager = self.make_rev_manager(doc_id, pool.clone())?;
        let server = Arc::new(RevisionServerImpl {
            token,
            server: self.cloud_service.clone(),
        });
        let doc_editor = ClientDocumentEditor::new(doc_id, user, rev_manager, self.ws_sender.clone(), server).await?;
        self.ws_receivers.add(doc_id, doc_editor.ws_handler());
        self.open_cache.insert(&doc_id, &doc_editor);
        Ok(doc_editor)
    }

    fn make_rev_manager(&self, doc_id: &str, pool: Arc<ConnectionPool>) -> Result<RevisionManager, FlowyError> {
        let user_id = self.user.user_id()?;
        let cache = Arc::new(RevisionCache::new(&user_id, doc_id, pool));
        Ok(RevisionManager::new(&user_id, doc_id, cache))
    }
}

struct RevisionServerImpl {
    token: String,
    server: Arc<dyn DocumentCloudService>,
}

impl RevisionCloudService for RevisionServerImpl {
    #[tracing::instrument(level = "debug", skip(self))]
    fn fetch_object(&self, user_id: &str, doc_id: &str) -> FutureResult<Vec<Revision>, FlowyError> {
        let params = DocumentId {
            doc_id: doc_id.to_string(),
        };
        let server = self.server.clone();
        let token = self.token.clone();
        let user_id = user_id.to_string();

        FutureResult::new(async move {
            match server.read_document(&token, params).await? {
                None => Err(FlowyError::record_not_found().context("Remote doesn't have this document")),
                Some(doc) => {
                    let delta_data = Bytes::from(doc.text.clone());
                    let doc_md5 = md5(&delta_data);
                    let revision =
                        Revision::new(&doc.doc_id, doc.base_rev_id, doc.rev_id, delta_data, &user_id, doc_md5);
                    Ok(vec![revision])
                },
            }
        })
    }
}

pub struct OpenDocCache {
    inner: DashMap<String, Arc<ClientDocumentEditor>>,
}

impl OpenDocCache {
    fn new() -> Self { Self { inner: DashMap::new() } }

    pub(crate) fn insert(&self, doc_id: &str, doc: &Arc<ClientDocumentEditor>) {
        if self.inner.contains_key(doc_id) {
            log::warn!("Doc:{} already exists in cache", doc_id);
        }
        self.inner.insert(doc_id.to_string(), doc.clone());
    }

    pub(crate) fn contains(&self, doc_id: &str) -> bool { self.inner.get(doc_id).is_some() }

    pub(crate) fn get(&self, doc_id: &str) -> Option<Arc<ClientDocumentEditor>> {
        if !self.contains(&doc_id) {
            return None;
        }
        let opened_doc = self.inner.get(doc_id).unwrap();
        Some(opened_doc.clone())
    }

    pub(crate) fn remove(&self, id: &str) {
        let doc_id = id.to_string();
        if let Some(editor) = self.get(id) {
            editor.stop()
        }
        self.inner.remove(&doc_id);
    }
}

#[tracing::instrument(level = "debug", skip(state_receiver, receivers))]
fn listen_ws_state_changed(mut state_receiver: WSStateReceiver, receivers: Arc<DocumentWSReceivers>) {
    tokio::spawn(async move {
        while let Ok(state) = state_receiver.recv().await {
            receivers.ws_connect_state_changed(&state).await;
        }
    });
}
