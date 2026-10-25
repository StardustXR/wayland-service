use std::{sync::Arc, time::Duration};

use directories::BaseDirs;
use gluon_ipc::{Handler, Node, RefExt, RefFsBinding, ToRef};
use rand::random;
use stardust_xr_panels::{
	panel_item_acceptor::PanelItemAcceptor,
	protected_panel_provider::{
		ProtectedPanelGetResult, ProtectedPanelProvider, ProtectedPanelProviderHandler,
	},
};
use tokio::sync::{Mutex, oneshot};

use crate::{
	DEFAULT_PANEL_SHELL_PATH,
	protocols::{
		core::seat::Seat,
		xdg::{backend::XdgBackend, toplevel::Toplevel},
	},
};

pub async fn spawn_default_panel_shell(
	spatial_token: &Option<String>,
	toplevel: &Arc<Toplevel>,
	seat: &Arc<Seat>,
) -> Option<Arc<Node<XdgBackend>>> {
	let path = DEFAULT_PANEL_SHELL_PATH.wait();
	let path_str = path.exists().then(|| path.to_str()).flatten()?;
	let name = format!("{:x}", random::<u64>());
	// TODO: should this come from the random device maybe?
	let secret = format!("{:x}", random::<u64>());

	let mut vars = Vec::with_capacity(5);
	vars.push(("SDXR_WL_DEFAULT_PANEL_SHELL".into(), "1".into()));
	vars.push((
		"SDXR_PROTECTED_PANEL".into(),
		format!("path:{name}:{secret}"),
	));
	if let Some(token) = spatial_token {
		vars.push(("STARDUST_STARTUP_TOKEN".into(), token.clone()));
	}
	if let Some(app_id) = toplevel.app_id() {
		vars.push(("SDXR_WL_APP_ID".into(), app_id));
	}
	if let Some(title) = toplevel.title() {
		vars.push(("SDXR_WL_TITLE".into(), title));
	}

	let (tx, rx) = oneshot::channel();

	let (_node, node_ref) = ProtectedPanelProvider::new_node(ProtectedProvider {
		return_info: Mutex::new(Some(tx)),
		secret,
		seat: seat.clone(),
		toplevel: toplevel.clone(),
	})
	.ok()?;
	let _fs_binding = RefFsBinding::new(
		node_ref.proxy().to_ref(),
		BaseDirs::new()?
			.runtime_dir()?
			.join("stardust-protected-panel-provider")
			.join(name),
	)
	.ok()?;

	protostar_launcher::launch(path_str.into(), vars).await;

	tokio::time::timeout(Duration::from_secs_f32(1.0), rx)
		.await
		.ok()?
		.ok()
}

#[derive(Handler)]
struct ProtectedProvider {
	return_info: Mutex<Option<oneshot::Sender<Arc<Node<XdgBackend>>>>>,
	secret: String,
	seat: Arc<Seat>,
	toplevel: Arc<Toplevel>,
}

impl ProtectedPanelProviderHandler for ProtectedProvider {
	async fn get_panel(
		&self,
		_ctx: gluon_ipc::Context,
		secret: String,
		acceptor: PanelItemAcceptor,
	) -> ProtectedPanelGetResult {
		if secret != self.secret {
			return ProtectedPanelGetResult::InvalidSecret;
		}
		let Some(sender) = self.return_info.lock().await.take() else {
			return ProtectedPanelGetResult::AlreadyConsumed;
		};
		let Some(obj) = XdgBackend::connect(acceptor, &self.seat, &self.toplevel).await else {
			return ProtectedPanelGetResult::InvalidAcceptor;
		};
		_ = sender.send(obj);
		ProtectedPanelGetResult::Success
	}
}
