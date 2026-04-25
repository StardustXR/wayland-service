use std::{fs::OpenOptions, path::Path, sync::Arc};

use binderbinder::binder_object::{BinderObject, BinderObjectOrRef, ToBinderObjectOrRef};
use gluon_wire::{GluonCtx, impl_transaction_handler};
use pion_binder::PionBinderDevice;
use stardust_xr_fusion::fields::FieldRef;
use stardust_xr_panel_item::protocol::{PanelItemAcceptor, PanelItemProviderHandler, SpatialRefId};
use tokio::sync::RwLock;

use crate::CLIENT;

pub static ACCEPTORS: RwLock<Vec<(FieldRef, PanelItemAcceptor)>> = RwLock::const_new(Vec::new());

#[derive(Debug)]
pub struct PanelItemProvider {}
impl PanelItemProvider {
    pub async fn setup(dev: &PionBinderDevice, path: &Path) -> BinderObject<Self> {
        let obj = dev.register_object(PanelItemProvider {});
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
    async fn register_acceptor(&self, _ctx: GluonCtx, acceptor: PanelItemAcceptor) {
        tokio::spawn(async move {
            let field = acceptor.get_field().await.unwrap();
            let field_ref = FieldRef::import(CLIENT.wait(), field.id).await.unwrap();
            ACCEPTORS.write().await.push((field_ref, acceptor.clone()));
            // TODO: move to proper query system to avoid this mem leak
            // tokio::spawn(async move {
            //     acceptor.death_or_drop().await;
            //     remove_acceptor(acceptor).await;
            // })
        });
    }

    async fn drop_acceptor(&self, _ctx: GluonCtx, acceptor: PanelItemAcceptor) {
        tokio::spawn(async move {
            remove_acceptor(acceptor).await;
        });
    }

    async fn startup_token_spatial_ref(
        &self,
        _ctx: GluonCtx,
        token: String,
        spatial_ref: SpatialRefId,
    ) {
    }
}

async fn remove_acceptor(acceptor: PanelItemAcceptor) {
    ACCEPTORS.write().await.retain(|(_, a)| !match (
        a.to_binder_object_or_ref(),
        acceptor.to_binder_object_or_ref(),
    ) {
        (BinderObjectOrRef::Ref(a), BinderObjectOrRef::Ref(b)) => Arc::ptr_eq(&a, &b),
        (BinderObjectOrRef::WeakRef(a), BinderObjectOrRef::WeakRef(b)) => Arc::ptr_eq(&a, &b),
        // Realistically if we have local panel item acceptors we have quite
        // a few more important issues than leaking them
        (BinderObjectOrRef::Object(_), BinderObjectOrRef::Object(_)) => false,
        (BinderObjectOrRef::WeakObject(_), BinderObjectOrRef::WeakObject(_)) => false,
        _ => false,
    });
}

impl_transaction_handler!(PanelItemProvider);
