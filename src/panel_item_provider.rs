use std::{fs::OpenOptions, path::Path, sync::Arc};

use binderbinder::{
    TransactionHandler,
    binder_object::{BinderObject, BinderObjectOrRef, ToBinderObjectOrRef},
};
use gluon_wire::{GluonDataReader, drop_tracking::DropNotifier};
use pion_binder::PionBinderDevice;
use stardust_xr_fusion::fields::FieldRef;
use stardust_xr_panel_item::protocol::{PanelItemAcceptor, PanelItemProviderHandler};
use tokio::sync::RwLock;

use crate::CLIENT;

pub static ACCEPTORS: RwLock<Vec<(FieldRef, PanelItemAcceptor)>> = RwLock::const_new(Vec::new());

#[derive(Debug)]
pub struct PanelItemProvider {
    drop_notifs: RwLock<Vec<DropNotifier>>,
}
impl PanelItemProvider {
    pub async fn setup(dev: &PionBinderDevice, path: &Path) -> Arc<BinderObject<Self>> {
        let obj = dev.register_object(PanelItemProvider {
            drop_notifs: RwLock::default(),
        });
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .create(false)
            .open(path)
            .unwrap();
        dev.bind_binder_ref_to_file(file, &obj).await.unwrap();
        obj
    }
}
impl PanelItemProviderHandler for PanelItemProvider {
    fn register_acceptor(&self, acceptor: PanelItemAcceptor) {
        tokio::spawn(async move {
            let field = acceptor.get_field().await;
            let field_ref = FieldRef::import(CLIENT.wait(), field.id).await.unwrap();
            ACCEPTORS.write().await.push((field_ref, acceptor));
        });
    }

    fn drop_acceptor(&self, acceptor: PanelItemAcceptor) {
        tokio::spawn(async move {
            ACCEPTORS.write().await.retain(|(_, a)| !match (
                a.to_binder_object_or_ref(),
                acceptor.to_binder_object_or_ref(),
            ) {
                (BinderObjectOrRef::Ref(a), BinderObjectOrRef::Ref(b)) => Arc::ptr_eq(&a, &b),
                (BinderObjectOrRef::WeakRef(a), BinderObjectOrRef::WeakRef(b)) => {
                    Arc::ptr_eq(&a, &b)
                }
                // Realistically if we have local panel item acceptors we have quite
                // a few more important issues than leaking them
                (BinderObjectOrRef::Object(_), BinderObjectOrRef::Object(_)) => false,
                (BinderObjectOrRef::WeakObject(_), BinderObjectOrRef::WeakObject(_)) => false,
                _ => false,
            });
        });
    }

    async fn drop_notification_requested(&self, notifier: gluon_wire::drop_tracking::DropNotifier) {
        self.drop_notifs.write().await.push(notifier);
    }
}

impl TransactionHandler for PanelItemProvider {
    async fn handle(
        &self,
        transaction: binderbinder::device::Transaction,
    ) -> binderbinder::payload::PayloadBuilder<'_> {
        let mut data = GluonDataReader::from_payload(transaction.payload);
        self.dispatch_two_way(transaction.code, &mut data)
            .await
            .to_payload()
    }

    async fn handle_one_way(&self, transaction: binderbinder::device::Transaction) {
        let mut data = GluonDataReader::from_payload(transaction.payload);
        self.dispatch_one_way(transaction.code, &mut data).await
    }
}
